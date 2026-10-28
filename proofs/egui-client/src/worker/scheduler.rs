use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::task::{AbortHandle, Id, JoinSet};

use super::{WorkerCommand, WorkerEvent, WorkerSink, WorkerState, execute_command, send_event};

pub(super) const CONTROL_CAPACITY: usize = 32;
const DISCONNECT_CAPACITY: usize = 32;
pub(super) const READ_CAPACITY: usize = 24;
pub(super) const ARTWORK_CAPACITY: usize = 12;
pub(super) const EVENT_CAPACITY: usize = 32;
const READ_CONCURRENCY: usize = 3;
const ARTWORK_CONCURRENCY: usize = 2;
const READ_KEYS: usize = 16;
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// Submission never waits for HTTP, queue space or event consumption.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerSubmitError {
    /// Nothing was queued. The caller must retain/retry or show the rejection.
    Busy,
    /// The worker is closing or its queue could not be accessed.
    Stopped,
}

impl std::fmt::Display for WorkerSubmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Busy => "The network queue is busy. Please retry.",
            Self::Stopped => "The network worker has stopped.",
        })
    }
}

/// Closing is best effort, not an acknowledgement of server-side completion.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WorkerShutdownReport {
    /// Accepted ordered commands abandoned after the bounded grace period.
    pub abandoned_commands: usize,
    /// The worker thread did not exit normally.
    pub worker_panicked: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Lane {
    Control,
    Read,
    Artwork,
}

#[derive(Clone, Copy)]
struct Ticket {
    lane: Lane,
    key: Option<usize>,
    revision: u64,
    epoch: u64,
}

struct Queued {
    command: WorkerCommand,
    ticket: Ticket,
}

#[derive(Default)]
struct QueueState {
    controls: VecDeque<Queued>,
    reads: VecDeque<Queued>,
    artwork: VecDeque<Queued>,
    active_photo_requests: HashSet<u64>,
    cancelled_photo_requests: HashSet<u64>,
    versions: [u64; READ_KEYS],
    epoch: u64,
    closing: bool,
}

#[derive(Default)]
pub(super) struct CommandQueue {
    state: Mutex<QueueState>,
    wake: Notify,
}

impl WorkerCommand {
    fn photo_request_id(&self) -> Option<u64> {
        match self {
            Self::LoadPhotoThumbnail { request_id, .. }
            | Self::LoadPhotoFullSize { request_id, .. } => Some(*request_id),
            _ => None,
        }
    }

    fn cancellation_event(&self, in_flight: bool) -> Option<WorkerEvent> {
        let message = if in_flight {
            "Cancelled by disconnect. The in-flight server operation may already have completed."
        } else {
            "Cancelled by disconnect before execution."
        }
        .to_owned();
        match self {
            Self::Connect { request_id, .. }
            | Self::ResumeSignIn { request_id, .. }
            | Self::QuickConnectStart { request_id, .. }
            | Self::QuickConnectAuthenticate { request_id, .. } => {
                Some(WorkerEvent::ConnectionFailed {
                    request_id: Some(*request_id),
                    message,
                })
            }
            Self::SetPlayed {
                request_id,
                item_id,
                ..
            } => Some(WorkerEvent::WatchedStateFailed {
                request_id: *request_id,
                item_id: item_id.clone(),
                message,
            }),
            Self::SetFavorite {
                request_id,
                item_id,
                ..
            } => Some(WorkerEvent::FavoriteStateFailed {
                request_id: *request_id,
                item_id: item_id.clone(),
                message,
            }),
            Self::PlaybackInfo { request_id, .. } => Some(WorkerEvent::PlaybackInfoFailed {
                request_id: *request_id,
                error_class: jellyfin_api::ErrorClass::Unexpected,
                retry_after: None,
            }),
            Self::PreparePlayback { request_id, .. } => Some(WorkerEvent::PlaybackPrepareFailed {
                request_id: *request_id,
                message,
            }),
            Self::ReportPlayback { request_id, .. } => Some(WorkerEvent::PlaybackReportFailed {
                request_id: *request_id,
                message,
            }),
            Self::QuickConnectPoll { request_id } => Some(WorkerEvent::QuickConnectPolled {
                request_id: *request_id,
                outcome: kattri_client_core::quick_connect_poll::PollOutcome::Error,
            }),
            // These are local idempotent barriers; disconnect also clears their secret.
            Self::QuickConnectCancel { .. } | Self::Disconnect | Self::Shutdown => None,
            _ => None,
        }
    }

    fn read_key(&self) -> Option<usize> {
        use super::{HomeCatalogKind, HomeRowKind};
        match self {
            Self::CheckServer { .. } => Some(0),
            Self::FetchItems { .. } => Some(1),
            Self::GetItem { .. } => Some(2),
            Self::LoadHomeRow { row, .. } => Some(match row {
                HomeRowKind::ContinueWatching => 3,
                HomeRowKind::NextUp => 4,
                HomeRowKind::Latest => 5,
            }),
            Self::LoadHomeCatalog { kind, .. } => Some(match kind {
                HomeCatalogKind::Libraries => 6,
                HomeCatalogKind::Collections => 7,
            }),
            Self::ListSeasons { .. } => Some(8),
            Self::ListEpisodes { .. } => Some(9),
            Self::ListSimilar { .. } => Some(10),
            Self::ListPersonItems { .. } => Some(11),
            Self::CheckQuickConnect { .. } => Some(12),
            _ => None,
        }
    }

    fn lane(&self) -> Lane {
        match self {
            Self::GetPersonArtwork { .. }
            | Self::GetPrimaryArtwork { .. }
            | Self::LoadPhotoThumbnail { .. }
            | Self::LoadPhotoFullSize { .. }
            | Self::GetBackdropArtwork { .. }
            | Self::GetLiveTvLogo { .. }
            | Self::MetadataThumbnail { .. } => Lane::Artwork,
            command if command.read_key().is_some() => Lane::Read,
            _ => Lane::Control,
        }
    }

    fn is_boundary(&self) -> bool {
        matches!(
            self,
            Self::Connect { .. }
                | Self::ResumeSignIn { .. }
                | Self::Disconnect
                | Self::QuickConnectStart { .. }
                | Self::QuickConnectAuthenticate { .. }
                | Self::QuickConnectCancel { .. }
        )
    }
}

impl CommandQueue {
    pub(super) fn send(&self, command: WorkerCommand) -> Result<(), WorkerSubmitError> {
        if matches!(command, WorkerCommand::Shutdown) {
            self.shutdown();
            return Ok(());
        }
        let mut state = self.state.lock().map_err(|_| WorkerSubmitError::Stopped)?;
        if state.closing {
            return Err(WorkerSubmitError::Stopped);
        }
        let lane = command.lane();
        let key = command.read_key();
        match lane {
            Lane::Control => {
                if matches!(command, WorkerCommand::Disconnect) {
                    // Idempotent consecutive barriers share one slot. Never evict a
                    // preceding mutation, authentication or playback command.
                    if state
                        .controls
                        .back()
                        .is_some_and(|entry| matches!(entry.command, WorkerCommand::Disconnect))
                    {
                        return Ok(());
                    }
                    if state.controls.len() >= CONTROL_CAPACITY + DISCONNECT_CAPACITY {
                        return Err(WorkerSubmitError::Busy);
                    }
                } else if state.controls.len() >= CONTROL_CAPACITY + DISCONNECT_CAPACITY
                    || state
                        .controls
                        .iter()
                        .filter(|entry| !matches!(entry.command, WorkerCommand::Disconnect))
                        .count()
                        >= CONTROL_CAPACITY
                {
                    return Err(WorkerSubmitError::Busy);
                }
            }
            Lane::Read => {
                if state.reads.len() >= READ_CAPACITY
                    && !state.reads.iter().any(|entry| entry.ticket.key == key)
                {
                    return Err(WorkerSubmitError::Busy);
                }
            }
            Lane::Artwork if state.artwork.len() >= ARTWORK_CAPACITY => {
                return Err(WorkerSubmitError::Busy);
            }
            Lane::Artwork => {}
        }
        if command.is_boundary() {
            Self::invalidate_reads(&mut state);
        }
        let revision = if let Some(key) = key {
            state.versions[key] = state.versions[key].wrapping_add(1);
            state.reads.retain(|entry| entry.ticket.key != Some(key));
            state.versions[key]
        } else {
            0
        };
        let queued = Queued {
            command,
            ticket: Ticket {
                lane,
                key,
                revision,
                epoch: state.epoch,
            },
        };
        match lane {
            Lane::Control => state.controls.push_back(queued),
            Lane::Read => state.reads.push_back(queued),
            Lane::Artwork => state.artwork.push_back(queued),
        }
        drop(state);
        self.wake.notify_one();
        Ok(())
    }

    fn invalidate_reads(state: &mut QueueState) {
        state.epoch = state.epoch.wrapping_add(1);
        state.reads.clear();
        state.artwork.clear();
    }

    /// Call only when the UI invalidates the corresponding read/loading state.
    pub(super) fn cancel_reads(&self) {
        if let Ok(mut state) = self.state.lock() {
            Self::invalidate_reads(&mut state);
        }
        self.wake.notify_one();
    }

    pub(super) fn cancel_navigation_reads(&self) {
        if let Ok(mut state) = self.state.lock() {
            for key in [1, 2, 8, 9, 10, 11] {
                state.versions[key] = state.versions[key].wrapping_add(1);
            }
            state
                .reads
                .retain(|entry| !matches!(entry.ticket.key, Some(1 | 2 | 8 | 9 | 10 | 11)));
        }
        self.wake.notify_one();
    }

    /// Remove queued photo jobs and signal active matching jobs for abort. Only
    /// active IDs are retained, so cancellation bookkeeping is bounded by the
    /// artwork concurrency limit; unrelated work is never selected by this path.
    pub(super) fn cancel_photo_image_requests(
        &self,
        request_ids: Vec<u64>,
    ) -> Result<(), WorkerSubmitError> {
        let mut state = self.state.lock().map_err(|_| WorkerSubmitError::Stopped)?;
        let request_ids: HashSet<u64> = request_ids.into_iter().collect();
        state.artwork.retain(|entry| {
            entry
                .command
                .photo_request_id()
                .is_none_or(|request_id| !request_ids.contains(&request_id))
        });
        let active_matches: Vec<_> = request_ids
            .into_iter()
            .filter(|request_id| state.active_photo_requests.contains(request_id))
            .collect();
        state.cancelled_photo_requests.extend(active_matches);
        drop(state);
        self.wake.notify_one();
        Ok(())
    }

    fn take_photo_cancellations(&self) -> HashSet<u64> {
        self.state.lock().map_or_else(
            |_| HashSet::new(),
            |mut state| std::mem::take(&mut state.cancelled_photo_requests),
        )
    }

    fn finish_photo_request(&self, request_id: Option<u64>) {
        if let Some(request_id) = request_id
            && let Ok(mut state) = self.state.lock()
        {
            state.active_photo_requests.remove(&request_id);
            state.cancelled_photo_requests.remove(&request_id);
        }
    }

    pub(super) fn shutdown(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.closing = true;
            Self::invalidate_reads(&mut state);
        }
        self.wake.notify_one();
    }

    fn current(&self, ticket: Ticket) -> bool {
        self.state.lock().is_ok_and(|state| {
            ticket.epoch == state.epoch
                && ticket
                    .key
                    .is_none_or(|key| ticket.revision == state.versions[key])
        })
    }

    fn closing(&self) -> bool {
        self.state.lock().map_or(true, |state| state.closing)
    }

    fn pop(&self, lane: Lane) -> Option<Queued> {
        let mut state = self.state.lock().ok()?;
        let queued = match lane {
            Lane::Control => state.controls.pop_front(),
            Lane::Read => state.reads.pop_front(),
            Lane::Artwork => state.artwork.pop_front(),
        };
        if let Some(request_id) = queued
            .as_ref()
            .and_then(|entry| entry.command.photo_request_id())
        {
            state.active_photo_requests.insert(request_id);
        }
        queued
    }

    fn pop_read(&self, allow_background: bool, prefer_background: bool) -> Option<Queued> {
        let mut state = self.state.lock().ok()?;
        let background = |entry: &Queued| matches!(entry.ticket.key, Some(3..=7));
        let preferred = prefer_background
            .then(|| state.reads.iter().position(background))
            .flatten();
        let position = preferred.or_else(|| {
            state
                .reads
                .iter()
                .position(|entry| allow_background || !background(entry))
        })?;
        state.reads.remove(position)
    }

    fn has_foreground_read(&self) -> bool {
        self.state.lock().is_ok_and(|state| {
            state
                .reads
                .iter()
                .any(|entry| !matches!(entry.ticket.key, Some(3..=7)))
        })
    }

    fn boundary_pending(&self) -> bool {
        self.state.lock().map_or(true, |state| {
            state
                .controls
                .iter()
                .any(|entry| entry.command.is_boundary())
        })
    }

    fn control_len(&self) -> usize {
        self.state.lock().map_or(0, |state| state.controls.len())
    }

    fn take_disconnect(&self) -> Option<Vec<Queued>> {
        let mut state = self.state.lock().ok()?;
        let position = state
            .controls
            .iter()
            .position(|entry| matches!(entry.command, WorkerCommand::Disconnect))?;
        let cancelled = state.controls.drain(..position).collect();
        state.controls.pop_front();
        Some(cancelled)
    }

    #[cfg(test)]
    pub(super) fn recv(&self) -> Result<WorkerCommand, ()> {
        self.pop(Lane::Control)
            .or_else(|| self.pop(Lane::Read))
            .or_else(|| self.pop(Lane::Artwork))
            .map(|entry| entry.command)
            .ok_or(())
    }
}

struct Active {
    ticket: Ticket,
    abort: AbortHandle,
    boundary: bool,
    invalidation: u64,
    cancellation: Option<WorkerEvent>,
    disconnected: bool,
    photo_request_id: Option<u64>,
}

struct Completed {
    state: WorkerState,
    reauthentication: bool,
}

pub(super) async fn run(
    queue: Arc<CommandQueue>,
    events: WorkerSink,
    device_id: String,
) -> WorkerShutdownReport {
    run_with_grace(queue, events, device_id, SHUTDOWN_GRACE).await
}

async fn run_with_grace(
    queue: Arc<CommandQueue>,
    events: WorkerSink,
    device_id: String,
    grace: Duration,
) -> WorkerShutdownReport {
    let mut state = WorkerState::default();
    let mut read_state = state.read_snapshot();
    let mut jobs = JoinSet::new();
    let mut active: HashMap<Id, Active> = HashMap::new();
    let mut closing_at = None;
    let mut invalidation = 0u64;
    loop {
        // Register before looking at the queues: enqueue/complete cannot lose a wake.
        let notified = queue.wake.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if queue.closing() && closing_at.is_none() {
            closing_at = Some(tokio::time::Instant::now() + grace);
        }
        if let Some(cancelled) = queue.take_disconnect() {
            // A local sign-out is an explicit cancellation boundary, not queue
            // pressure. Acknowledge every affected ordered HTTP operation.
            invalidation = invalidation.wrapping_add(1);
            state.clear_authentication();
            read_state.clear_authentication();
            for job in active.values_mut() {
                job.abort.abort();
                job.disconnected = true;
                if let Some(event) = job.cancellation.take() {
                    send_event(&events, &events.ctx, event).await;
                }
            }
            for entry in cancelled {
                if let Some(event) = entry.command.cancellation_event(false) {
                    send_event(&events, &events.ctx, event).await;
                }
            }
        }
        for request_id in queue.take_photo_cancellations() {
            for job in active
                .values()
                .filter(|job| job.photo_request_id == Some(request_id))
            {
                job.abort.abort();
            }
        }
        for job in active.values() {
            if job.ticket.lane != Lane::Control && !queue.current(job.ticket) {
                job.abort.abort();
            }
        }
        if !active.values().any(|job| job.ticket.lane == Lane::Control)
            && let Some(queued) = queue.pop(Lane::Control)
        {
            let boundary = queued.command.is_boundary();
            let cancellation = queued.command.cancellation_event(true);
            let photo_request_id = queued.command.photo_request_id();
            let mut command_state = std::mem::take(&mut state);
            let sink = events.for_request();
            let device = device_id.clone();
            let abort = jobs.spawn(async move {
                let authenticated = command_state.session.is_some();
                execute_command(queued.command, &mut command_state, &sink, &device).await;
                let reauthentication = authenticated
                    && (command_state.session.is_none() || sink.authentication_expired())
                    && !boundary;
                Completed {
                    state: command_state,
                    reauthentication,
                }
            });
            active.insert(
                abort.id(),
                Active {
                    ticket: queued.ticket,
                    abort,
                    boundary,
                    invalidation,
                    cancellation,
                    disconnected: false,
                    photo_request_id,
                },
            );
        }
        let boundary = queue.boundary_pending() || active.values().any(|job| job.boundary);
        if closing_at.is_none() && !boundary {
            for (lane, limit) in [
                (Lane::Read, READ_CONCURRENCY),
                (Lane::Artwork, ARTWORK_CONCURRENCY),
            ] {
                let count = active
                    .values()
                    .filter(|job| job.ticket.lane == lane)
                    .count();
                for _ in count..limit {
                    let background_count = active
                        .values()
                        .filter(|job| {
                            job.ticket.lane == Lane::Read && matches!(job.ticket.key, Some(3..=7))
                        })
                        .count();
                    let queued = if lane == Lane::Read {
                        let active_reads = active
                            .values()
                            .filter(|job| job.ticket.lane == Lane::Read)
                            .count();
                        let has_foreground = queue.has_foreground_read();
                        queue.pop_read(
                            background_count == 0 && !has_foreground,
                            active_reads >= 2 && background_count == 0,
                        )
                    } else {
                        queue.pop(lane)
                    };
                    let Some(queued) = queued else { break };
                    let photo_request_id = queued.command.photo_request_id();
                    let mut command_state = read_state.read_snapshot();
                    let sink = events.for_request();
                    let device = device_id.clone();
                    let abort = jobs.spawn(async move {
                        let authenticated = command_state.session.is_some();
                        execute_command(queued.command, &mut command_state, &sink, &device).await;
                        let reauthentication = authenticated
                            && (command_state.session.is_none() || sink.authentication_expired());
                        Completed {
                            state: command_state,
                            reauthentication,
                        }
                    });
                    active.insert(
                        abort.id(),
                        Active {
                            ticket: queued.ticket,
                            abort,
                            boundary: false,
                            invalidation,
                            cancellation: None,
                            disconnected: false,
                            photo_request_id,
                        },
                    );
                }
            }
        }
        if closing_at.is_some() && active.is_empty() && queue.control_len() == 0 {
            return WorkerShutdownReport::default();
        }
        let deadline =
            closing_at.unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(86400));
        tokio::select! {
            _ = &mut notified => {},
            result = jobs.join_next_with_id(), if !active.is_empty() => {
                match result {
                    Some(Ok((id, completed))) => if let Some(job) = active.remove(&id) {
                        queue.finish_photo_request(job.photo_request_id);
                        let current = !job.disconnected && (job.ticket.lane == Lane::Control || queue.current(job.ticket));
                        if completed.reauthentication && current {
                            invalidation = invalidation.wrapping_add(1);
                            queue.cancel_reads();
                            state.clear_authentication();
                            read_state.clear_authentication();
                        }
                        if job.ticket.lane == Lane::Control && !job.disconnected {
                            state = completed.state;
                            if job.invalidation != invalidation { state.clear_authentication(); }
                            read_state = state.read_snapshot();
                        }
                    },
                    Some(Err(error)) => {
                        let job = active.remove(&error.id());
                        queue.finish_photo_request(job.as_ref().and_then(|job| job.photo_request_id));
                        if !error.is_cancelled() {
                            queue.shutdown();
                            let abandoned = queue.control_len() + usize::from(job.is_some_and(|job| job.ticket.lane == Lane::Control));
                            jobs.abort_all();
                            return WorkerShutdownReport { abandoned_commands: abandoned, worker_panicked: true };
                        }
                    }
                    None => {}
                }
            },
            _ = tokio::time::sleep_until(deadline), if closing_at.is_some() => {
                let abandoned_commands = queue.control_len() + active.values().filter(|job| job.ticket.lane == Lane::Control).count();
                jobs.abort_all();
                while jobs.join_next().await.is_some() {}
                return WorkerShutdownReport { abandoned_commands, worker_panicked: false };
            }
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod photo_cancellation_tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Condvar, Mutex};
    use std::thread::{self, JoinHandle};
    use std::time::Duration;
    use tokio::sync::mpsc as tokio_mpsc;

    fn photo(request_id: u64) -> WorkerCommand {
        WorkerCommand::LoadPhotoThumbnail {
            request_id,
            item_id: format!("photo-{request_id}"),
            image_tag: None,
            photo_index: request_id as usize,
            visible: true,
        }
    }

    #[test]
    fn queued_photo_cancellation_removes_only_matching_photo_jobs() {
        let queue = CommandQueue::default();
        queue.send(photo(10)).expect("queue photo");
        queue.send(photo(11)).expect("queue second photo");
        queue
            .send(WorkerCommand::GetPrimaryArtwork {
                request_id: 12,
                item_id: "poster".into(),
                image_tag: None,
            })
            .expect("queue unrelated artwork");
        queue
            .send(WorkerCommand::GetItem {
                request_id: 13,
                item_id: "metadata".into(),
            })
            .expect("queue unrelated API read");

        queue
            .cancel_photo_image_requests(vec![10])
            .expect("cancel matching queued photo");

        let state = queue.state.lock().expect("queue lock");
        assert_eq!(state.artwork.len(), 2);
        assert!(!state.artwork.iter().any(|entry| {
            matches!(
                entry.command,
                WorkerCommand::LoadPhotoThumbnail { request_id: 10, .. }
            )
        }));
        assert!(state.artwork.iter().any(|entry| {
            matches!(
                entry.command,
                WorkerCommand::LoadPhotoThumbnail { request_id: 11, .. }
            )
        }));
        assert!(state.artwork.iter().any(|entry| {
            matches!(
                entry.command,
                WorkerCommand::GetPrimaryArtwork { request_id: 12, .. }
            )
        }));
        assert_eq!(state.reads.len(), 1);
        assert!(state.cancelled_photo_requests.is_empty());
    }

    #[test]
    fn active_photo_cancellation_targets_only_matching_photo_request() {
        let queue = CommandQueue::default();
        queue.send(photo(20)).expect("queue first photo");
        queue.send(photo(21)).expect("queue second photo");
        queue
            .send(WorkerCommand::GetPrimaryArtwork {
                request_id: 22,
                item_id: "poster".into(),
                image_tag: None,
            })
            .expect("queue unrelated artwork");

        let first = queue.pop(Lane::Artwork).expect("start first photo");
        let second = queue.pop(Lane::Artwork).expect("start second photo");
        let unrelated = queue.pop(Lane::Artwork).expect("start artwork");
        assert_eq!(first.command.photo_request_id(), Some(20));
        assert_eq!(second.command.photo_request_id(), Some(21));
        assert_eq!(unrelated.command.photo_request_id(), None);

        queue
            .cancel_photo_image_requests(vec![20, 22, 999])
            .expect("cancel matching active photo");

        assert_eq!(queue.take_photo_cancellations(), HashSet::from([20]));
        assert!(queue.take_photo_cancellations().is_empty());
        let state = queue.state.lock().expect("queue lock");
        assert_eq!(state.active_photo_requests, HashSet::from([20, 21]));
        assert!(state.cancelled_photo_requests.is_empty());
    }

    struct RequestGate {
        released: Mutex<bool>,
        changed: Condvar,
    }

    impl RequestGate {
        fn new() -> Self {
            Self {
                released: Mutex::new(false),
                changed: Condvar::new(),
            }
        }

        fn wait(&self) {
            let mut released = self.released.lock().expect("gate lock");
            while !*released {
                released = self.changed.wait(released).expect("gate wait");
            }
        }

        fn release(&self) {
            *self.released.lock().expect("gate lock") = true;
            self.changed.notify_all();
        }
    }

    struct FixtureState {
        requests: tokio_mpsc::UnboundedSender<String>,
        gates: Mutex<HashMap<String, Arc<RequestGate>>>,
        stopping: AtomicBool,
        accepted: AtomicUsize,
        request_lines: AtomicUsize,
        seen_paths: Mutex<Vec<String>>,
    }

    struct PhotoFixture {
        url: String,
        requests: tokio_mpsc::UnboundedReceiver<String>,
        state: Arc<FixtureState>,
        listener: Option<JoinHandle<()>>,
    }

    impl PhotoFixture {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind photo fixture");
            listener
                .set_nonblocking(true)
                .expect("set nonblocking listener");
            let url = format!("http://{}", listener.local_addr().expect("fixture address"));
            let (request_tx, requests) = tokio_mpsc::unbounded_channel();
            let state = Arc::new(FixtureState {
                requests: request_tx,
                gates: Mutex::new(HashMap::new()),
                stopping: AtomicBool::new(false),
                accepted: AtomicUsize::new(0),
                request_lines: AtomicUsize::new(0),
                seen_paths: Mutex::new(Vec::new()),
            });
            let thread_state = state.clone();
            let listener_thread = thread::spawn(move || {
                while !thread_state.stopping.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            thread_state.accepted.fetch_add(1, Ordering::Relaxed);
                            let state = thread_state.clone();
                            thread::spawn(move || serve_photo_fixture(stream, state));
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::park_timeout(Duration::from_millis(1));
                        }
                        Err(_) => break,
                    }
                }
            });
            Self {
                url,
                requests,
                state,
                listener: Some(listener_thread),
            }
        }

        fn gate(&self, item_id: &str) -> Arc<RequestGate> {
            let gate = Arc::new(RequestGate::new());
            self.state
                .gates
                .lock()
                .expect("fixture gate map")
                .insert(item_id.into(), gate.clone());
            gate
        }
    }

    impl Drop for PhotoFixture {
        fn drop(&mut self) {
            if let Ok(gates) = self.state.gates.lock() {
                for gate in gates.values() {
                    gate.release();
                }
            }
            self.state.stopping.store(true, Ordering::Release);
            let _ = TcpStream::connect(self.url.trim_start_matches("http://"));
            if let Some(listener) = self.listener.take() {
                let _ = listener.join();
            }
        }
    }

    fn serve_photo_fixture(mut stream: TcpStream, state: Arc<FixtureState>) {
        let _ = stream.set_nonblocking(false);
        let Ok(cloned) = stream.try_clone() else {
            return;
        };
        let mut reader = BufReader::new(cloned);
        let mut first_line = String::new();
        if reader.read_line(&mut first_line).is_err() || first_line.is_empty() {
            return;
        }
        state.request_lines.fetch_add(1, Ordering::Relaxed);
        let path = first_line
            .split_whitespace()
            .nth(1)
            .unwrap_or("/")
            .to_owned();
        if let Ok(mut paths) = state.seen_paths.lock() {
            paths.push(path.clone());
        }
        let mut content_length = 0;
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).is_err() || header == "\r\n" || header.is_empty() {
                break;
            }
            if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                content_length = value.trim().parse::<usize>().unwrap_or(0);
            }
        }
        let mut body = vec![0; content_length];
        if content_length != 0 && reader.read_exact(&mut body).is_err() {
            return;
        }
        let _ = state.requests.send(path.clone());

        let gated_item = ["photo-cancel", "photo-keep"]
            .into_iter()
            .find(|item_id| path.contains(item_id));
        if let Some(item_id) = gated_item
            && let Some(gate) = state
                .gates
                .lock()
                .ok()
                .and_then(|gates| gates.get(item_id).cloned())
        {
            gate.wait();
        }

        let (status, response) = if path.contains("/System/Info/Public") {
            (
                "200 OK",
                r#"{"ServerName":"Fixture","Version":"1.0","Id":"fixture"}"#,
            )
        } else if path.contains("/Users/AuthenticateByName") {
            (
                "200 OK",
                r#"{"AccessToken":"fixture-token","ServerId":"fixture","User":{"Id":"user-1","Name":"fixture"}}"#,
            )
        } else if path.contains("/Items") && !path.contains("/Images/") {
            (
                "200 OK",
                r#"{"Items":[],"TotalRecordCount":0,"StartIndex":0}"#,
            )
        } else {
            ("404 Not Found", "")
        };
        let header = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            response.len()
        );
        let _ = stream.write_all(header.as_bytes());
        let _ = stream.write_all(response.as_bytes());
    }

    async fn next_path(
        requests: &mut tokio_mpsc::UnboundedReceiver<String>,
        contains: &str,
        queue: &CommandQueue,
        fixture: &FixtureState,
    ) -> String {
        let result = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let path = requests.recv().await.expect("fixture request stream");
                if path.contains(contains) {
                    return path;
                }
            }
        })
        .await;
        result.unwrap_or_else(|_| {
            let state = queue.state.lock().expect("photo queue state");
            panic!(
                "timed out waiting for fixture request containing {contains}; accepted_connections={}, request_lines={}, seen_paths={:?}, active={:?}, queued_artwork={}, cancelled={:?}",
                fixture.accepted.load(Ordering::Relaxed),
                fixture.request_lines.load(Ordering::Relaxed),
                fixture.seen_paths.lock().expect("seen fixture paths"),
                state.active_photo_requests,
                state.artwork.len(),
                state.cancelled_photo_requests
            )
        })
    }

    async fn next_matching_event(
        events: &mut tokio_mpsc::Receiver<WorkerEvent>,
        label: &str,
        matches: impl Fn(&WorkerEvent) -> bool,
    ) -> WorkerEvent {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let event = events.recv().await.expect("worker event stream");
                if matches(&event) {
                    return event;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for worker event: {label}"))
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scheduler_aborts_only_the_cancelled_active_photo_future() {
        let mut fixture = PhotoFixture::start();
        let cancel_gate = fixture.gate("photo-cancel");
        let keep_gate = fixture.gate("photo-keep");
        let queue = Arc::new(CommandQueue::default());
        let (event_sender, mut events) = tokio_mpsc::channel(EVENT_CAPACITY);
        let sink = WorkerSink::for_test(event_sender);
        let worker_queue = queue.clone();
        let worker = tokio::spawn(run_with_grace(
            worker_queue,
            sink,
            "photo-cancel-test".into(),
            Duration::from_secs(2),
        ));

        queue
            .send(WorkerCommand::Connect {
                request_id: 1,
                server_url: fixture.url.clone(),
                username: "fixture".into(),
                password: zeroize::Zeroizing::new("fixture".into()),
                allow_insecure_remote_http: false,
                saved: crate::worker::SavedSignInRequest::not_remembered(),
            })
            .expect("enqueue fixture connection");
        assert!(
            next_path(
                &mut fixture.requests,
                "System/Info/Public",
                &queue,
                &fixture.state
            )
            .await
            .contains("System/Info/Public")
        );
        let checked = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("timed out waiting for initial worker event")
            .expect("worker event stream closed");
        match checked {
            WorkerEvent::CheckingServer { request_id: 1 } => {}
            WorkerEvent::ConnectionFailed { message, .. } => {
                panic!("fixture connection failed before checking: {message}");
            }
            _ => panic!("worker did not begin the fixture connection"),
        }
        let connected = next_matching_event(&mut events, "fixture catalog", |event| {
            matches!(
                event,
                WorkerEvent::ItemsLoaded { request_id: 1, .. }
                    | WorkerEvent::ConnectionFailed {
                        request_id: Some(1),
                        ..
                    }
                    | WorkerEvent::ItemsFailed { request_id: 1, .. }
            )
        })
        .await;
        match connected {
            WorkerEvent::ItemsLoaded { request_id: 1, .. } => {}
            WorkerEvent::ConnectionFailed { message, .. } => {
                panic!("fixture connection failed: {message}");
            }
            WorkerEvent::ItemsFailed { error_class, .. } => {
                panic!("fixture library request failed: {error_class:?}");
            }
            _ => panic!("worker did not finish loading the fixture library"),
        }

        queue
            .send(WorkerCommand::LoadPhotoThumbnail {
                request_id: 100,
                item_id: "photo-cancel".into(),
                image_tag: None,
                photo_index: 0,
                visible: true,
            })
            .expect("enqueue cancellable photo");
        queue
            .send(WorkerCommand::LoadPhotoFullSize {
                request_id: 101,
                item_id: "photo-keep".into(),
                image_tag: None,
                photo_index: 1,
                visible: true,
            })
            .expect("enqueue photo to preserve");
        queue
            .send(WorkerCommand::GetPrimaryArtwork {
                request_id: 102,
                item_id: "poster-keep".into(),
                image_tag: None,
            })
            .expect("enqueue unrelated artwork");

        let (cancel_path, keep_path) = tokio::time::timeout(Duration::from_secs(5), async {
            let (mut cancel_path, mut keep_path) = (None, None);
            while cancel_path.is_none() || keep_path.is_none() {
                let path = fixture
                    .requests
                    .recv()
                    .await
                    .expect("photo fixture request stream");
                if path.contains("photo-cancel") {
                    cancel_path = Some(path);
                } else if path.contains("photo-keep") {
                    keep_path = Some(path);
                }
            }
            (cancel_path, keep_path)
        })
        .await
        .expect("both concurrent photo requests reach the fixture");
        assert!(cancel_path.expect("cancel request").contains("Images"));
        assert!(keep_path.expect("preserved request").contains("Images"));

        queue
            .cancel_photo_image_requests(vec![100])
            .expect("cancel active photo request");

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let active = queue
                    .state
                    .lock()
                    .expect("photo queue state")
                    .active_photo_requests
                    .contains(&100);
                if !active {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancelled request must release its artwork slot");

        // The third request cannot start until the scheduler observes the abort and
        // removes that active job, proving the cancellation reached run_with_grace.
        assert!(
            next_path(&mut fixture.requests, "poster-keep", &queue, &fixture.state)
                .await
                .contains("Images")
        );
        cancel_gate.release();
        keep_gate.release();
        let (preserved_photo, preserved_art) =
            tokio::time::timeout(Duration::from_secs(5), async {
                let (mut photo, mut artwork) = (false, false);
                while !photo || !artwork {
                    match events.recv().await.expect("worker event stream") {
                        WorkerEvent::PhotoFullSizeFailed {
                            request_id: 101, ..
                        } => photo = true,
                        WorkerEvent::PrimaryArtworkFailed { request_id: 102 } => artwork = true,
                        _ => {}
                    }
                }
                (photo, artwork)
            })
            .await
            .expect("timed out waiting for preserved image jobs");
        assert!(preserved_photo && preserved_art);

        queue.shutdown();
        tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .expect("scheduler shut down")
            .expect("scheduler task joined");
        assert!(
            events.try_recv().is_err(),
            "cancelled photo emitted a completion"
        );
    }
}

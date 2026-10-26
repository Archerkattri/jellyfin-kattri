use super::*;
use crate::worker::{HomeRowKind, WorkerCommand, WorkerEvent, WorkerSink};
use jellyfin_api::{ItemQuery, PlaybackProgressInfo};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tokio::sync::mpsc as tokio_mpsc;

fn fetch(id: u64, term: &str) -> WorkerCommand {
    WorkerCommand::FetchItems {
        request_id: id,
        query: ItemQuery {
            search_term: Some(term.into()),
            ..ItemQuery::default()
        },
    }
}

#[test]
fn rapid_searches_keep_only_latest_key_and_invalidate_started_ticket() {
    let queue = CommandQueue::default();
    queue.send(fetch(1, "one")).expect("enqueue first search");
    let first = queue.pop(Lane::Read).expect("first search queued");
    queue.send(fetch(2, "two")).expect("enqueue replacement");
    assert!(!queue.current(first.ticket));
    queue
        .send(fetch(3, "three"))
        .expect("enqueue latest search");

    assert!(
        matches!(queue.pop(Lane::Read).map(|entry| entry.command), Some(WorkerCommand::FetchItems { request_id: 3, query }) if query.search_term.as_deref() == Some("three"))
    );
    assert!(queue.pop(Lane::Read).is_none());
}

#[test]
fn background_read_can_run_when_no_foreground_read_is_waiting() {
    let queue = CommandQueue::default();
    queue
        .send(WorkerCommand::LoadHomeRow {
            request_id: 1,
            row: HomeRowKind::Latest,
        })
        .expect("queue background row");
    let queued = queue
        .pop_read(!queue.has_foreground_read(), false)
        .expect("do not starve a lone background request");
    assert!(matches!(queued.command, WorkerCommand::LoadHomeRow { .. }));
}

#[test]
fn full_control_fifo_rejects_without_dropping_and_reserves_disconnect() {
    let queue = CommandQueue::default();
    for id in 0..CONTROL_CAPACITY {
        queue
            .send(WorkerCommand::SetPlayed {
                request_id: id as u64,
                item_id: format!("item-{id}"),
                played: id % 2 == 0,
            })
            .expect("control capacity accepts every slot");
    }
    assert_eq!(
        queue.send(WorkerCommand::SetPlayed {
            request_id: 99,
            item_id: "rejected".into(),
            played: true,
        }),
        Err(WorkerSubmitError::Busy),
    );
    queue
        .send(WorkerCommand::Disconnect)
        .expect("reserved disconnect slot");

    for id in 0..CONTROL_CAPACITY {
        assert!(
            matches!(queue.pop(Lane::Control).map(|entry| entry.command), Some(WorkerCommand::SetPlayed { request_id, .. }) if request_id == id as u64)
        );
    }
    assert!(matches!(
        queue.pop(Lane::Control).map(|entry| entry.command),
        Some(WorkerCommand::Disconnect)
    ));
    assert!(queue.pop(Lane::Control).is_none());
}

#[test]
fn disconnect_barrier_reserve_is_bounded_and_pressure_is_reported() {
    let queue = CommandQueue::default();
    for index in 0..DISCONNECT_CAPACITY {
        queue
            .send(WorkerCommand::Disconnect)
            .expect("reserved barrier");
        queue
            .send(WorkerCommand::SetPlayed {
                request_id: index as u64,
                item_id: format!("item-{index}"),
                played: true,
            })
            .expect("ordinary slot remains available");
    }
    assert!(matches!(
        queue.send(WorkerCommand::Disconnect),
        Err(WorkerSubmitError::Busy)
    ));
    assert_eq!(queue.state.lock().expect("queue lock").controls.len(), 64);
}

#[test]
fn disconnect_and_read_cancellation_invalidate_queued_reads_and_artwork() {
    let queue = CommandQueue::default();
    queue.send(fetch(1, "obsolete")).expect("queue search");
    queue
        .send(WorkerCommand::GetPrimaryArtwork {
            request_id: 2,
            item_id: "poster".into(),
            image_tag: None,
        })
        .expect("queue artwork");
    let read_ticket = queue
        .state
        .lock()
        .expect("queue lock")
        .reads
        .front()
        .expect("read ticket")
        .ticket;
    let artwork_ticket = queue
        .state
        .lock()
        .expect("queue lock")
        .artwork
        .front()
        .expect("art ticket")
        .ticket;
    queue
        .send(WorkerCommand::Disconnect)
        .expect("queue barrier");

    assert!(!queue.current(read_ticket));
    assert!(!queue.current(artwork_ticket));
    assert!(queue.state.lock().expect("queue lock").reads.is_empty());
    assert!(queue.state.lock().expect("queue lock").artwork.is_empty());

    queue
        .send(fetch(3, "cancel me"))
        .expect("queue another search");
    queue.cancel_reads();
    assert!(queue.state.lock().expect("queue lock").reads.is_empty());
    assert!(queue.boundary_pending());

    queue
        .send(fetch(4, "navigation"))
        .expect("queue navigation read");
    queue
        .send(WorkerCommand::LoadHomeRow {
            request_id: 5,
            row: HomeRowKind::Latest,
        })
        .expect("queue background home read");
    queue
        .send(WorkerCommand::CheckServer {
            request_id: 6,
            server_url: "http://127.0.0.1:8096".into(),
            allow_insecure_remote_http: false,
        })
        .expect("queue server probe");
    queue.cancel_navigation_reads();
    let state = queue.state.lock().expect("queue lock");
    assert!(state.reads.iter().all(|entry| entry.ticket.key != Some(1)));
    assert!(state.reads.iter().any(|entry| entry.ticket.key == Some(5)));
    assert!(state.reads.iter().any(|entry| entry.ticket.key == Some(0)));
}

#[test]
fn artwork_admission_is_bounded_and_rejects_without_eviction() {
    let queue = CommandQueue::default();
    for id in 0..ARTWORK_CAPACITY {
        queue
            .send(WorkerCommand::GetPrimaryArtwork {
                request_id: id as u64,
                item_id: format!("poster-{id}"),
                image_tag: None,
            })
            .expect("artwork slot available");
    }
    assert_eq!(
        queue.send(WorkerCommand::GetPrimaryArtwork {
            request_id: 99,
            item_id: "overflow".into(),
            image_tag: None,
        }),
        Err(WorkerSubmitError::Busy),
    );
    let queued = queue.state.lock().expect("queue lock").artwork.len();
    assert_eq!(queued, ARTWORK_CAPACITY);
    for id in 0..ARTWORK_CAPACITY {
        assert!(
            matches!(queue.pop(Lane::Artwork).map(|entry| entry.command), Some(WorkerCommand::GetPrimaryArtwork { request_id, .. }) if request_id == id as u64)
        );
    }
}

struct Gate {
    state: Mutex<(bool, bool)>,
    changed: Condvar,
}

impl Gate {
    fn new() -> Self {
        Self {
            state: Mutex::new((false, false)),
            changed: Condvar::new(),
        }
    }

    fn wait(&self) {
        let mut state = self.state.lock().expect("gate lock");
        state.1 = true;
        self.changed.notify_all();
        while !state.0 {
            state = self.changed.wait(state).expect("gate wait");
        }
    }

    fn wait_until_entered(&self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut state = self.state.lock().expect("gate lock");
        while !state.1 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "request did not reach its HTTP gate");
            let (next, timeout) = self
                .changed
                .wait_timeout(state, remaining)
                .expect("gate entry wait");
            state = next;
            assert!(
                !timeout.timed_out() || state.1,
                "request did not reach its HTTP gate"
            );
        }
    }

    fn release(&self) {
        self.state.lock().expect("gate lock").0 = true;
        self.changed.notify_all();
    }
}

#[derive(Default)]
struct HttpControl {
    gate_path: Mutex<Option<String>>,
    gate: Mutex<Option<Arc<Gate>>>,
    active_reads: AtomicUsize,
    max_reads: AtomicUsize,
    entered: Mutex<Vec<String>>,
    entered_changed: Condvar,
}

struct LocalServer {
    url: String,
    control: Arc<HttpControl>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl LocalServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local fixture");
        listener.set_nonblocking(true).expect("nonblocking accept");
        let url = format!("http://{}", listener.local_addr().expect("local address"));
        let control = Arc::new(HttpControl::default());
        let stop = Arc::new(AtomicBool::new(false));
        let thread_control = control.clone();
        let thread_stop = stop.clone();
        let thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let control = thread_control.clone();
                        thread::spawn(move || serve_one(stream, control));
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
            control,
            stop,
            thread: Some(thread),
        }
    }

    fn pause(&self, path_fragment: &str) -> Arc<Gate> {
        let gate = Arc::new(Gate::new());
        *self.control.gate_path.lock().expect("gate path lock") = Some(path_fragment.into());
        *self.control.gate.lock().expect("gate lock") = Some(gate.clone());
        gate
    }

    fn wait_entered(&self, expected: usize) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut entered = self.control.entered.lock().expect("entered lock");
        while entered.len() < expected {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out waiting for local HTTP requests"
            );
            let (next, timeout) = self
                .control
                .entered_changed
                .wait_timeout(entered, remaining)
                .expect("entered wait");
            entered = next;
            assert!(
                !timeout.timed_out() || entered.len() >= expected,
                "timed out waiting for local HTTP requests"
            );
        }
        entered.clone()
    }
}

impl Drop for LocalServer {
    fn drop(&mut self) {
        if let Ok(gate) = self.control.gate.lock()
            && let Some(gate) = gate.as_ref()
        {
            gate.release();
        }
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.join().expect("stop accept loop");
        }
    }
}

fn serve_one(mut stream: TcpStream, control: Arc<HttpControl>) {
    // The listener is non-blocking and Windows passes that mode to accepted sockets, so a
    // request read before its bytes arrive would fail with WouldBlock and be dropped.
    stream
        .set_nonblocking(false)
        .expect("blocking fixture stream");
    let mut reader = BufReader::new(stream.try_clone().expect("clone fixture stream"));
    let mut first_line = String::new();
    if reader.read_line(&mut first_line).is_err() || first_line.is_empty() {
        return;
    }
    let path = first_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_owned();
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
    if reader.read_exact(&mut body).is_err() && content_length != 0 {
        return;
    }

    let is_read = path.contains("/Items")
        || path.contains("/QuickConnect/Enabled")
        || path.contains("/System/Info/Public");
    // Count the read before announcing the request: a test that wakes on `entered` and then
    // asserts on `max_reads` must not see the announcement ahead of the count.
    if is_read {
        let active = control.active_reads.fetch_add(1, Ordering::AcqRel) + 1;
        control.max_reads.fetch_max(active, Ordering::AcqRel);
    }
    control
        .entered
        .lock()
        .expect("entered lock")
        .push(path.clone());
    control.entered_changed.notify_all();

    let should_pause = control
        .gate_path
        .lock()
        .expect("gate path lock")
        .as_ref()
        .is_some_and(|fragment| fragment == "READ" && is_read || path.contains(fragment));
    // Clone the gate out first: a guard held across wait() would block LocalServer::drop
    // when an assertion fails before the test releases the gate.
    let gate = control.gate.lock().expect("gate lock").clone();
    if should_pause && let Some(gate) = gate {
        gate.wait();
    }
    if is_read {
        control.active_reads.fetch_sub(1, Ordering::AcqRel);
    }

    let (status, response) = if path.contains("/System/Info/Public") {
        (
            "200 OK",
            r#"{"ServerName":"Fixture","Version":"1.0","Id":"local"}"#,
        )
    } else if path.contains("/Users/AuthenticateByName") {
        (
            "200 OK",
            r#"{"AccessToken":"local-token","ServerId":"local","User":{"Id":"user-1","Name":"fixture"}}"#,
        )
    } else if path.contains("/QuickConnect/Enabled") {
        ("200 OK", "true")
    } else if path.contains("/Items") {
        (
            "200 OK",
            r#"{"Items":[],"TotalRecordCount":0,"StartIndex":0}"#,
        )
    } else {
        ("204 No Content", "")
    };
    let bytes = response.as_bytes();
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        bytes.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(bytes);
}

fn sink() -> (WorkerSink, tokio_mpsc::Receiver<WorkerEvent>) {
    let (sender, receiver) = tokio_mpsc::channel(EVENT_CAPACITY);
    (WorkerSink::for_test(sender), receiver)
}

fn run_on_thread(
    queue: Arc<CommandQueue>,
    events: WorkerSink,
    grace: Duration,
) -> JoinHandle<WorkerShutdownReport> {
    thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build test runtime");
        runtime.block_on(run_with_grace(
            queue,
            events,
            "scheduler-test-device".into(),
            grace,
        ))
    })
}

#[test]
fn scheduler_caps_reads_at_three_and_keeps_control_and_queued_read_moving() {
    let server = LocalServer::start();
    let queue = Arc::new(CommandQueue::default());
    let (events, mut receiver) = sink();
    let worker = run_on_thread(queue.clone(), events, Duration::from_secs(2));
    queue
        .send(WorkerCommand::Connect {
            request_id: 1,
            server_url: server.url.clone(),
            username: "fixture".into(),
            password: zeroize::Zeroizing::new("synthetic".into()),
            allow_insecure_remote_http: false,
            saved: crate::worker::SavedSignInRequest::not_remembered(),
        })
        .expect("connect to synthetic fixture");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if matches!(
            receiver.try_recv(),
            Ok(WorkerEvent::ItemsLoaded { request_id: 1, .. })
        ) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "fixture connection did not complete"
        );
        thread::yield_now();
    }

    server.control.entered.lock().expect("entered lock").clear();
    let gate = server.pause("READ");
    queue.send(fetch(10, "slow-a")).expect("enqueue first read");
    queue
        .send(WorkerCommand::CheckServer {
            request_id: 11,
            server_url: server.url.clone(),
            allow_insecure_remote_http: false,
        })
        .expect("enqueue second read");
    queue
        .send(WorkerCommand::CheckQuickConnect {
            request_id: 12,
            server_url: server.url.clone(),
            allow_insecure_remote_http: false,
        })
        .expect("enqueue third read");
    queue
        .send(WorkerCommand::GetItem {
            request_id: 13,
            item_id: "fixture-item".into(),
        })
        .expect("queue fourth foreground read");
    queue
        .send(WorkerCommand::LoadHomeRow {
            request_id: 14,
            row: HomeRowKind::Latest,
        })
        .expect("queue background read");
    let paths = server.wait_entered(3);
    // A home request gets a reserved slot; the fourth foreground request waits.
    let started = server.control.entered.lock().expect("entered lock").len();
    assert_eq!(started, 3);
    assert_eq!(server.control.max_reads.load(Ordering::Acquire), 3);
    assert!(
        paths
            .iter()
            .any(|path| path.contains("/Items") && path.contains("searchTerm=slow-a"))
    );
    assert!(
        paths
            .iter()
            .any(|path| path.contains("/System/Info/Public"))
    );
    assert!(paths.iter().any(|path| path.contains("/Items/Latest")));
    queue
        .send(WorkerCommand::ReportPlayback {
            request_id: 14,
            action: kattri_client_core::playback_reporting::PlaybackReportAction::Started(
                PlaybackProgressInfo::new("movie"),
            ),
        })
        .expect("submit ordered playback report while reads wait");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if matches!(
            receiver.try_recv(),
            Ok(WorkerEvent::PlaybackReportSent { request_id: 14 })
        ) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "ordered playback report waited behind held reads"
        );
        thread::yield_now();
    }
    gate.release();
    let completed_paths = server.wait_entered(6);
    assert!(
        completed_paths
            .iter()
            .any(|path| path.contains("/QuickConnect/Enabled"))
    );
    assert!(
        completed_paths
            .iter()
            .any(|path| path.contains("fixture-item"))
    );
    queue.shutdown();
    let report = worker.join().expect("worker exits");
    assert_eq!(report, WorkerShutdownReport::default());
}

#[test]
fn newer_search_cancels_slow_inflight_http_read_without_waiting_for_its_response() {
    let server = LocalServer::start();
    let queue = Arc::new(CommandQueue::default());
    let (events, mut receiver) = sink();
    let worker = run_on_thread(queue.clone(), events, Duration::from_secs(2));
    queue
        .send(WorkerCommand::Connect {
            request_id: 1,
            server_url: server.url.clone(),
            username: "fixture".into(),
            password: zeroize::Zeroizing::new("synthetic".into()),
            allow_insecure_remote_http: false,
            saved: crate::worker::SavedSignInRequest::not_remembered(),
        })
        .expect("connect to synthetic fixture");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if matches!(
            receiver.try_recv(),
            Ok(WorkerEvent::ItemsLoaded { request_id: 1, .. })
        ) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "fixture connection did not complete"
        );
        thread::yield_now();
    }

    server.control.entered.lock().expect("entered lock").clear();
    let old_search_gate = server.pause("searchTerm=old");
    queue.send(fetch(2, "old")).expect("enqueue slow search");
    let old_path = server.wait_entered(1);
    assert!(old_path[0].contains("searchTerm=old"));
    old_search_gate.wait_until_entered();

    queue
        .send(fetch(3, "newest"))
        .expect("replace in-flight search");
    let paths = server.wait_entered(2);
    assert!(paths.iter().any(|path| path.contains("searchTerm=newest")));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if matches!(
            receiver.try_recv(),
            Ok(WorkerEvent::ItemsLoaded { request_id: 3, .. })
        ) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "new search waited for obsolete HTTP response"
        );
        thread::yield_now();
    }

    old_search_gate.release();
    queue.shutdown();
    assert_eq!(
        worker.join().expect("worker exits"),
        WorkerShutdownReport::default()
    );
}

#[test]
fn disconnect_cancels_inflight_and_queued_controls_then_retains_work_after_barrier() {
    let server = LocalServer::start();
    let queue = Arc::new(CommandQueue::default());
    let (events, mut receiver) = sink();
    let worker = run_on_thread(queue.clone(), events, Duration::from_secs(2));
    queue
        .send(WorkerCommand::Connect {
            request_id: 1,
            server_url: server.url.clone(),
            username: "fixture".into(),
            password: zeroize::Zeroizing::new("synthetic".into()),
            allow_insecure_remote_http: false,
            saved: crate::worker::SavedSignInRequest::not_remembered(),
        })
        .expect("connect to synthetic fixture");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if matches!(
            receiver.try_recv(),
            Ok(WorkerEvent::ItemsLoaded { request_id: 1, .. })
        ) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "fixture connection did not complete"
        );
        thread::yield_now();
    }

    server.control.entered.lock().expect("entered lock").clear();
    let inflight_gate = server.pause("/Sessions/Playing");
    queue
        .send(WorkerCommand::ReportPlayback {
            request_id: 20,
            action: kattri_client_core::playback_reporting::PlaybackReportAction::Started(
                PlaybackProgressInfo::new("in-flight-item"),
            ),
        })
        .expect("start slow protected playback report");
    server.wait_entered(1);
    inflight_gate.wait_until_entered();
    queue
        .send(WorkerCommand::SetPlayed {
            request_id: 21,
            item_id: "queued-item".into(),
            played: true,
        })
        .expect("queue protected mutation");
    queue
        .send(WorkerCommand::ReportPlayback {
            request_id: 22,
            action: kattri_client_core::playback_reporting::PlaybackReportAction::Started(
                PlaybackProgressInfo::new("queued-report"),
            ),
        })
        .expect("queue protected playback report");
    queue
        .send(WorkerCommand::Disconnect)
        .expect("enqueue urgent disconnect barrier");
    queue
        .send(WorkerCommand::Connect {
            request_id: 23,
            server_url: server.url.clone(),
            username: "fixture".into(),
            password: zeroize::Zeroizing::new("synthetic".into()),
            allow_insecure_remote_http: false,
            saved: crate::worker::SavedSignInRequest::not_remembered(),
        })
        .expect("queue reconnect after disconnect barrier");

    let mut inflight_failed = false;
    let mut queued_failed = false;
    let mut queued_report_failed = false;
    let mut reconnected = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while !(inflight_failed && queued_failed && queued_report_failed && reconnected) {
        assert!(
            Instant::now() < deadline,
            "disconnect did not report cancellations and run post-barrier connect"
        );
        match receiver.try_recv() {
            Ok(WorkerEvent::PlaybackReportFailed {
                request_id: 20,
                message,
            }) => {
                assert!(message.contains("server operation may already have completed"));
                inflight_failed = true;
            }
            Ok(WorkerEvent::WatchedStateFailed {
                request_id: 21,
                message,
                ..
            }) => {
                assert!(message.contains("before execution"));
                queued_failed = true;
            }
            Ok(WorkerEvent::PlaybackReportFailed {
                request_id: 22,
                message,
            }) => {
                assert!(message.contains("before execution"));
                queued_report_failed = true;
            }
            Ok(WorkerEvent::ItemsLoaded { request_id: 23, .. }) => reconnected = true,
            _ => thread::yield_now(),
        }
    }
    let paths = server.control.entered.lock().expect("entered lock").clone();
    assert!(paths.iter().any(|path| path.contains("/Sessions/Playing")));
    assert!(!paths.iter().any(|path| path.contains("queued-item")));
    assert_eq!(
        paths
            .iter()
            .filter(|path| path.contains("/Sessions/Playing"))
            .count(),
        1
    );

    inflight_gate.release();
    queue.shutdown();
    assert_eq!(
        worker.join().expect("worker exits"),
        WorkerShutdownReport::default()
    );
}

#[test]
fn configured_shutdown_grace_reports_abandoned_ordered_work_and_fast_shutdown_flushes() {
    let server = LocalServer::start();
    let gate = server.pause("/Users/AuthenticateByName");
    let queue = Arc::new(CommandQueue::default());
    let (events, _receiver) = sink();
    let worker = run_on_thread(queue.clone(), events, Duration::from_millis(80));
    queue
        .send(WorkerCommand::Connect {
            request_id: 1,
            server_url: server.url.clone(),
            username: "fixture".into(),
            password: zeroize::Zeroizing::new("synthetic".into()),
            allow_insecure_remote_http: false,
            saved: crate::worker::SavedSignInRequest::not_remembered(),
        })
        .expect("start connect");
    server.wait_entered(2);
    gate.wait_until_entered();
    queue
        .send(WorkerCommand::ReportPlayback {
            request_id: 2,
            action: kattri_client_core::playback_reporting::PlaybackReportAction::Started(
                PlaybackProgressInfo::new("movie"),
            ),
        })
        .expect("queue ordered playback report");
    queue.shutdown();
    let report = worker.join().expect("grace-expired worker exits");
    assert_eq!(report.abandoned_commands, 2);
    assert!(!report.worker_panicked);
    gate.release();

    let queue = Arc::new(CommandQueue::default());
    let (events, _receiver) = sink();
    let worker = run_on_thread(queue.clone(), events, Duration::from_secs(1));
    queue
        .send(WorkerCommand::ReportPlayback {
            request_id: 3,
            action: kattri_client_core::playback_reporting::PlaybackReportAction::Started(
                PlaybackProgressInfo::new("no-session"),
            ),
        })
        .expect("accept report before shutdown");
    queue.shutdown();
    assert_eq!(
        worker.join().expect("flushed worker exits"),
        WorkerShutdownReport::default()
    );
}

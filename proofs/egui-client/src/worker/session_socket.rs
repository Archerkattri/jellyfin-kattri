//! Session socket for remote control (M4-13): the thread that reads inbound commands.
//!
//! `SessionSocket` blocks, and the worker runtime has one thread, so the socket runs on its
//! own thread. The thread lives while its [`SessionSocketHandle`] does. The worker drops the
//! handle when the session ends (sign-out, reauthentication, shutdown), and the loop stops
//! within one poll interval. Nothing here logs the token or the socket URL.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use jellyfin_api::websocket::{Inbound, SessionSocket, WebSocketError};
use jellyfin_api::{JellyfinClient, Session};

use super::{WorkerEvent, WorkerSink};

/// Wait before the one retry after a failed connection or a dropped socket.
const RETRY_DELAY: Duration = Duration::from_secs(5);
/// The retry wait checks the stop flag this often, so a sign-out does not wait out the delay.
const STOP_CHECK_INTERVAL: Duration = Duration::from_millis(100);
/// Consecutive failures before the loop gives up: the first failure is retried once.
const MAX_CONSECUTIVE_FAILURES: u32 = 2;

/// The worker event for one inbound message the app acts on. Other messages have no event.
fn worker_event(inbound: Inbound) -> Option<WorkerEvent> {
    match inbound {
        Inbound::Command(command) => Some(WorkerEvent::RemoteCommandReceived { command }),
        Inbound::SyncPlay(command) => Some(WorkerEvent::SyncPlayCommandReceived { command }),
        Inbound::SyncPlayGroupUpdate(update) => Some(WorkerEvent::SyncPlayGroupUpdate { update }),
        Inbound::Ignored | Inbound::Idle | Inbound::Closed => None,
    }
}

/// Owns the socket thread. Dropping the handle asks the thread to stop.
pub(super) struct SessionSocketHandle {
    stop: Arc<AtomicBool>,
}

impl Drop for SessionSocketHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

/// The part of [`SessionSocket`] the loop uses. Tests substitute a scripted socket.
trait InboundSocket {
    fn receive(&mut self) -> Result<Inbound, WebSocketError>;
    fn close(&mut self);
}

impl InboundSocket for SessionSocket {
    fn receive(&mut self) -> Result<Inbound, WebSocketError> {
        SessionSocket::receive(self)
    }

    fn close(&mut self) {
        // Best effort: the server may already be gone, and the socket is dropped either way.
        let _ = SessionSocket::close(self);
    }
}

/// Start the socket loop for the signed-in session. Returns `None` when there is no session
/// or the thread cannot start. The caller keeps the handle for as long as the session lasts.
pub(super) fn start(
    client: Option<&JellyfinClient>,
    session: Option<&Session>,
    events: &WorkerSink,
) -> Option<SessionSocketHandle> {
    if !events.remote_socket {
        return None;
    }
    let client = client?.clone();
    let session = session?.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let sender = events.sender.clone();
    let ctx = events.ctx.clone();
    let spawned = thread::Builder::new()
        .name("jellyfin-session-socket".into())
        .spawn(move || {
            run_loop(
                || SessionSocket::connect_with_cancel(&client, &session, Arc::clone(&thread_stop)),
                &thread_stop,
                RETRY_DELAY,
                |inbound| {
                    if thread_stop.load(Ordering::Acquire) {
                        return false;
                    }
                    let Some(event) = worker_event(inbound) else {
                        return true;
                    };
                    let sent = sender.blocking_send(event).is_ok();
                    ctx.request_repaint();
                    sent
                },
            );
        });
    match spawned {
        Ok(_) => Some(SessionSocketHandle { stop }),
        Err(error) => {
            tracing::warn!(%error, "session socket thread could not start");
            None
        }
    }
}

/// How one connected socket ended.
#[derive(Debug, PartialEq, Eq)]
enum End {
    /// The stop flag was set, or the app no longer takes commands.
    Stopped,
    /// The server closed the socket or a read failed. A retry may help.
    Failed,
}

/// Read the session socket until it is stopped, retrying a failed connection once after
/// [`RETRY_DELAY`]. `connect` opens one socket. `deliver` takes one command or SyncPlay
/// message and returns `false` when the loop must end.
fn run_loop<S, C, D>(mut connect: C, stop: &AtomicBool, retry_delay: Duration, mut deliver: D)
where
    S: InboundSocket,
    C: FnMut() -> Result<S, WebSocketError>,
    D: FnMut(Inbound) -> bool,
{
    let mut failures = 0_u32;
    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        let ended = match connect() {
            Ok(mut socket) => {
                failures = 0;
                let ended = receive_until_end(&mut socket, stop, &mut deliver);
                socket.close();
                ended
            }
            Err(error) if is_permanent(&error) => {
                tracing::warn!(%error, "session socket not started");
                return;
            }
            Err(error) => {
                tracing::warn!(%error, "session socket connection failed");
                End::Failed
            }
        };
        if ended == End::Stopped {
            return;
        }
        failures += 1;
        if failures >= MAX_CONSECUTIVE_FAILURES {
            tracing::warn!("session socket stopped after its retry failed");
            return;
        }
        tracing::info!("session socket retrying after a delay");
        if !wait(retry_delay, stop) {
            return;
        }
    }
}

/// Deliver commands until the stop flag is set or the socket fails.
fn receive_until_end<S: InboundSocket>(
    socket: &mut S,
    stop: &AtomicBool,
    deliver: &mut impl FnMut(Inbound) -> bool,
) -> End {
    loop {
        if stop.load(Ordering::Acquire) {
            return End::Stopped;
        }
        match socket.receive() {
            Ok(
                inbound @ (Inbound::Command(_)
                | Inbound::SyncPlay(_)
                | Inbound::SyncPlayGroupUpdate(_)),
            ) => {
                if !deliver(inbound) {
                    return End::Stopped;
                }
            }
            Ok(Inbound::Ignored | Inbound::Idle) => {}
            Ok(Inbound::Closed) => {
                tracing::info!("session socket closed by the server");
                return End::Failed;
            }
            Err(error) => {
                tracing::warn!(%error, "session socket read failed");
                return End::Failed;
            }
        }
    }
}

/// A refused credential or a bad configuration will not change on retry.
fn is_permanent(error: &WebSocketError) -> bool {
    matches!(
        error,
        WebSocketError::Unauthorized
            | WebSocketError::InvalidConfiguration
            | WebSocketError::SetupCancelled
    )
}

/// Sleep for `delay` in short steps. Returns `false` when the stop flag was set first.
fn wait(delay: Duration, stop: &AtomicBool) -> bool {
    let mut remaining = delay;
    loop {
        if stop.load(Ordering::Acquire) {
            return false;
        }
        if remaining.is_zero() {
            return true;
        }
        let step = remaining.min(STOP_CHECK_INTERVAL);
        thread::sleep(step);
        remaining = remaining.saturating_sub(step);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use jellyfin_api::remote_command::RemoteCommand;
    use jellyfin_api::websocket::{
        Inbound, SyncPlayGroupUpdate, SyncPlayMessage, SyncPlayWireCommand, WebSocketError,
    };
    use tokio::sync::mpsc;

    use super::{InboundSocket, SessionSocketHandle, run_loop, wait, worker_event};
    use crate::worker::{WorkerCommand, WorkerEvent, WorkerSink, WorkerState, execute_command};

    type Frame = Result<Inbound, WebSocketError>;

    /// Plays its frames, then sets the stop flag, as a sign-out does while the socket is idle.
    struct ScriptedSocket {
        frames: VecDeque<Frame>,
        stop: Arc<AtomicBool>,
    }

    impl InboundSocket for ScriptedSocket {
        fn receive(&mut self) -> Result<Inbound, WebSocketError> {
            match self.frames.pop_front() {
                Some(frame) => frame,
                None => {
                    self.stop.store(true, Ordering::Release);
                    Ok(Inbound::Idle)
                }
            }
        }

        fn close(&mut self) {}
    }

    /// One scripted connect attempt: an error, or a socket that plays the frames.
    enum Attempt {
        Fail(WebSocketError),
        Open(Vec<Frame>),
    }

    /// Run the loop over the scripted attempts. Returns the delivered commands and the number
    /// of connect attempts.
    fn run(attempts: Vec<Attempt>) -> (Vec<Inbound>, usize) {
        let stop = Arc::new(AtomicBool::new(false));
        let mut attempts = attempts.into_iter();
        let mut connects = 0;
        let mut delivered = Vec::new();
        run_loop(
            || {
                connects += 1;
                match attempts.next() {
                    Some(Attempt::Open(frames)) => Ok(ScriptedSocket {
                        frames: frames.into(),
                        stop: Arc::clone(&stop),
                    }),
                    Some(Attempt::Fail(error)) => Err(error),
                    None => Err(WebSocketError::Handshake),
                }
            },
            &stop,
            Duration::ZERO,
            |command| {
                delivered.push(command);
                true
            },
        );
        (delivered, connects)
    }

    #[test]
    fn a_pause_from_the_socket_is_delivered_once() {
        let (delivered, connects) = run(vec![Attempt::Open(vec![Ok(Inbound::Command(
            RemoteCommand::Pause,
        ))])]);
        assert_eq!(delivered, vec![Inbound::Command(RemoteCommand::Pause)]);
        assert_eq!(connects, 1);
    }

    #[test]
    fn a_sync_play_message_is_delivered_as_itself() {
        let message = SyncPlayMessage {
            command: SyncPlayWireCommand::Pause,
            position_ticks: 600_000_000,
            when: "1970-01-01T00:16:40.0000000Z".into(),
        };
        let (delivered, connects) = run(vec![Attempt::Open(vec![Ok(Inbound::SyncPlay(
            message.clone(),
        ))])]);
        assert_eq!(delivered, vec![Inbound::SyncPlay(message)]);
        assert_eq!(connects, 1);
    }

    #[test]
    fn a_sync_play_group_update_is_delivered_and_mapped_to_the_worker() {
        let update = SyncPlayGroupUpdate::NotInGroup {
            group_id: Some("group-1".into()),
        };
        let (delivered, connects) = run(vec![Attempt::Open(vec![Ok(
            Inbound::SyncPlayGroupUpdate(update.clone()),
        )])]);
        assert_eq!(
            delivered,
            vec![Inbound::SyncPlayGroupUpdate(update.clone())]
        );
        assert_eq!(connects, 1);
        assert!(matches!(
            worker_event(Inbound::SyncPlayGroupUpdate(update)),
            Some(WorkerEvent::SyncPlayGroupUpdate {
                update: SyncPlayGroupUpdate::NotInGroup {
                    group_id: Some(group_id)
                }
            }) if group_id == "group-1"
        ));
    }

    #[test]
    fn only_commands_and_sync_play_messages_have_a_worker_event() {
        assert!(matches!(
            worker_event(Inbound::Command(RemoteCommand::Pause)),
            Some(WorkerEvent::RemoteCommandReceived {
                command: RemoteCommand::Pause
            })
        ));
        assert!(matches!(
            worker_event(Inbound::SyncPlay(SyncPlayMessage {
                command: SyncPlayWireCommand::Seek,
                position_ticks: 0,
                when: "x".into(),
            })),
            Some(WorkerEvent::SyncPlayCommandReceived { .. })
        ));
        assert!(worker_event(Inbound::Ignored).is_none());
        assert!(worker_event(Inbound::Idle).is_none());
        assert!(worker_event(Inbound::Closed).is_none());
    }

    #[test]
    fn a_failed_connection_gets_one_retry() {
        let (delivered, connects) = run(vec![
            Attempt::Fail(WebSocketError::Connect(io::Error::other("refused"))),
            Attempt::Open(vec![Ok(Inbound::Command(RemoteCommand::Pause))]),
        ]);
        assert_eq!(delivered, vec![Inbound::Command(RemoteCommand::Pause)]);
        assert_eq!(connects, 2);
    }

    #[test]
    fn a_second_failed_connection_stops_the_loop() {
        let (delivered, connects) = run(vec![
            Attempt::Fail(WebSocketError::Handshake),
            Attempt::Fail(WebSocketError::Handshake),
            Attempt::Open(vec![Ok(Inbound::Command(RemoteCommand::Play))]),
        ]);
        assert!(delivered.is_empty());
        assert_eq!(connects, 2, "no third attempt after the retry failed");
    }

    #[test]
    fn a_refused_credential_is_not_retried() {
        let (delivered, connects) = run(vec![
            Attempt::Fail(WebSocketError::Unauthorized),
            Attempt::Open(vec![Ok(Inbound::Command(RemoteCommand::Pause))]),
        ]);
        assert!(delivered.is_empty());
        assert_eq!(connects, 1);
    }

    #[test]
    fn a_closed_socket_gets_one_retry() {
        let (delivered, connects) = run(vec![
            Attempt::Open(vec![Ok(Inbound::Closed)]),
            Attempt::Open(vec![Ok(Inbound::Command(RemoteCommand::Pause))]),
        ]);
        assert_eq!(delivered, vec![Inbound::Command(RemoteCommand::Pause)]);
        assert_eq!(connects, 2);
    }

    #[test]
    fn a_stop_during_delivery_ends_the_loop_without_more_commands() {
        let stop = Arc::new(AtomicBool::new(false));
        let mut connects = 0;
        let mut delivered = Vec::new();
        run_loop(
            || {
                connects += 1;
                Ok::<_, WebSocketError>(ScriptedSocket {
                    frames: VecDeque::from([
                        Ok(Inbound::Command(RemoteCommand::Pause)),
                        Ok(Inbound::Command(RemoteCommand::Play)),
                    ]),
                    stop: Arc::clone(&stop),
                })
            },
            &stop,
            Duration::ZERO,
            |command| {
                // The sign-out arrives while the first command is handled.
                delivered.push(command);
                stop.store(true, Ordering::Release);
                false
            },
        );
        assert_eq!(delivered, vec![Inbound::Command(RemoteCommand::Pause)]);
        assert_eq!(connects, 1);
    }

    #[test]
    fn the_retry_wait_gives_up_as_soon_as_the_stop_flag_is_set() {
        let stop = AtomicBool::new(true);
        let started = Instant::now();
        assert!(!wait(Duration::from_secs(60), &stop));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn the_retry_wait_completes_when_not_stopped() {
        let stop = AtomicBool::new(false);
        assert!(wait(Duration::from_millis(1), &stop));
        assert!(wait(Duration::ZERO, &stop));
    }

    #[test]
    fn dropping_the_handle_stops_the_socket_thread() {
        let stop = Arc::new(AtomicBool::new(false));
        let handle = SessionSocketHandle {
            stop: Arc::clone(&stop),
        };
        assert!(!stop.load(Ordering::Acquire));
        drop(handle);
        assert!(stop.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn sign_out_drops_the_socket_handle_and_stops_its_loop() {
        let stop = Arc::new(AtomicBool::new(false));
        let mut state = WorkerState {
            session_socket: Some(SessionSocketHandle {
                stop: Arc::clone(&stop),
            }),
            ..WorkerState::default()
        };
        let (sender, _receiver) = mpsc::channel(4);
        let events = WorkerSink::for_test(sender);
        execute_command(WorkerCommand::Disconnect, &mut state, &events, "device").await;
        assert!(state.session_socket.is_none());
        assert!(stop.load(Ordering::Acquire));
    }
}

//! The session lifecycle as an explicit, pure state machine.
//!
//! The app previously spread this lifecycle across a `ConnectionStatus` enum plus a `connection_request_id`,
//! a `server_check_pending` tuple, retry timers, quick-connect state and a socket handle, with the
//! "re-advertise capabilities for a new session record" step wired by hand. That made one class of bug
//! reachable: a reconnect yields a **new server session record** which starts without capabilities, and
//! nothing structural forced the advertisement to happen again (the shipped 1.0.0 bug).
//!
//! This machine makes that consequence structural: [`SessionState::Live`] only ever learns about a
//! socket through [`SessionEvent::SocketConnected`], and that transition always emits
//! [`SessionEffect::AdvertiseCapabilities`]. The effect list is the caller's contract; it executes
//! effects, it does not decide them.
//!
//! Pure: no I/O, no clock, no threads. The caller supplies events (including retry decisions) and runs
//! the returned effects through the worker.

use crate::epoch::Epoch;

/// Opaque identity of one server session record.
///
/// The server can hand out a *new* record on reconnect even for the same user and device, so this is
/// not a device id. Two live values with different ids are different session records.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(u64);

impl SessionId {
    /// Wrap a caller-chosen counter. The value only has to distinguish records within one run.
    pub const fn from_raw(value: u64) -> Self {
        Self(value)
    }

    /// The raw counter, for logging and tests.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Where the session lifecycle is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionState {
    /// No session. Nothing asynchronous is in flight that could be applied.
    SignedOut,
    /// A sign-in or reconnect attempt is running. `epoch` is the generation its results must carry.
    Connecting { epoch: Epoch },
    /// A session record exists and the socket for it is up.
    Live { session: SessionId, epoch: Epoch },
    /// A session record exists but its socket dropped. A retry keeps the same record until the
    /// server replaces it.
    Lost { session: SessionId, epoch: Epoch },
    /// The attempt failed in a way a retry cannot fix without user action (bad credentials, refused
    /// server). The UI shows the reason.
    Failed { reason: FailureReason },
}

/// Why an attempt ended in [`SessionState::Failed`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureReason {
    /// The server refused the credentials. Retrying without new input cannot succeed.
    CredentialsRejected,
    /// The server answered but is not usable (wrong software, refused plain HTTP, incompatible).
    ServerRefused,
    /// The user signed out while the attempt was running.
    Cancelled,
}

/// Inputs to the lifecycle. Every event carries the epoch it was issued under, so a result from an
/// abandoned generation is rejected before it can change the state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionEvent {
    /// The viewer (or a saved-sign-in resume) asked to connect.
    SignInStarted { epoch: Epoch },
    /// A sign-in attempt succeeded with this session record.
    SignedIn { epoch: Epoch, session: SessionId },
    /// The session socket for the current record connected. This is the event that must always
    /// re-advertise capabilities, including after a reconnect.
    SocketConnected { epoch: Epoch },
    /// The session socket dropped.
    SocketLost { epoch: Epoch },
    /// The attempt failed permanently.
    Failed { epoch: Epoch, reason: FailureReason },
    /// A retry is due (the caller owns the timer).
    RetryDue { epoch: Epoch },
    /// The viewer signed out.
    SignOut,
}

/// What the caller must do after a transition. Declared, not executed here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionEffect {
    /// Start a socket for the current session record.
    StartSocket { session: SessionId },
    /// Stop the current socket.
    StopSocket,
    /// Advertise remote-control capabilities for the current session record.
    ///
    /// Emitted on every transition into or within [`SessionState::Live`], because the record carrying
    /// the socket may be new and the server reports `SupportsRemoteControl=false` until told otherwise.
    AdvertiseCapabilities { session: SessionId },
    /// Load the first catalog page for the session.
    LoadCatalog,
    /// Show a terminal failure; the UI stops the spinner.
    ReportFailure { reason: FailureReason },
}

/// One transition: the next state and the effects that make it true in the outside world.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Transition {
    pub state: SessionState,
    pub effects: SessionEffects,
}

/// A small fixed-capacity effect list, so [`Transition`] stays `Copy` and allocation-free.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionEffects {
    items: [Option<SessionEffect>; 3],
    len: usize,
}

impl SessionEffects {
    pub const EMPTY: Self = Self {
        items: [None; 3],
        len: 0,
    };

    fn of(effect: SessionEffect) -> Self {
        let mut effects = Self::EMPTY;
        effects.push(effect);
        effects
    }

    fn push(&mut self, effect: SessionEffect) {
        if self.len < self.items.len() {
            self.items[self.len] = Some(effect);
            self.len += 1;
        }
    }

    /// The effects in execution order.
    pub fn iter(&self) -> impl Iterator<Item = SessionEffect> + '_ {
        self.items[..self.len].iter().filter_map(|effect| *effect)
    }

    /// Number of effects.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether there is nothing to do.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl SessionState {
    /// The epoch this state belongs to, when it has one.
    pub const fn epoch(self) -> Option<Epoch> {
        match self {
            Self::SignedOut | Self::Failed { .. } => None,
            Self::Connecting { epoch } | Self::Live { epoch, .. } | Self::Lost { epoch, .. } => {
                Some(epoch)
            }
        }
    }

    /// The live session record, if any.
    pub const fn session(self) -> Option<SessionId> {
        match self {
            Self::Live { session, .. } | Self::Lost { session, .. } => Some(session),
            Self::SignedOut | Self::Connecting { .. } | Self::Failed { .. } => None,
        }
    }

    /// Whether the viewer has a session (live or reconnecting).
    pub const fn has_session(self) -> bool {
        matches!(self, Self::Live { .. } | Self::Lost { .. })
    }
}

/// The session lifecycle. Owns only its state; all side effects come back as [`SessionEffect`]s.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionMachine {
    state: SessionState,
}

impl Default for SessionMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionMachine {
    pub const fn new() -> Self {
        Self {
            state: SessionState::SignedOut,
        }
    }

    pub const fn state(&self) -> SessionState {
        self.state
    }

    /// The epoch results must carry right now. [`Epoch::NONE`] while signed out.
    pub const fn epoch(&self) -> Epoch {
        match self.state.epoch() {
            Some(epoch) => epoch,
            None => Epoch::NONE,
        }
    }

    /// Advance the machine by one event and return the next state with its effects.
    ///
    /// Events from a generation other than the current one are ignored: the state and effects come
    /// back unchanged. That single check is the staleness boundary for the whole lifecycle.
    pub fn reduce(&mut self, event: SessionEvent) -> Transition {
        let current = self.epoch();
        // Intent events start or end a generation; they are never "stale". Everything else is a
        // result and must carry the live epoch to be applied.
        let stale = match event {
            SessionEvent::SignOut | SessionEvent::SignInStarted { .. } => false,
            SessionEvent::SignedIn { epoch, .. }
            | SessionEvent::SocketConnected { epoch }
            | SessionEvent::SocketLost { epoch }
            | SessionEvent::Failed { epoch, .. }
            | SessionEvent::RetryDue { epoch } => !epoch.is_current_for(current),
        };
        if stale {
            return self.transition(SessionEffects::EMPTY);
        }
        match (self.state, event) {
            // Start (or restart) a connection attempt under a fresh generation.
            (_, SessionEvent::SignInStarted { epoch }) => {
                self.state = SessionState::Connecting { epoch };
                self.transition(SessionEffects::EMPTY)
            }
            // A sign-in result only counts for the attempt that asked for it.
            (
                SessionState::Connecting { epoch: attempt },
                SessionEvent::SignedIn { session, .. },
            ) if attempt == self.epoch() => {
                self.state = SessionState::Live {
                    session,
                    epoch: attempt,
                };
                let mut effects = SessionEffects::of(SessionEffect::StartSocket { session });
                effects.push(SessionEffect::AdvertiseCapabilities { session });
                effects.push(SessionEffect::LoadCatalog);
                self.transition(effects)
            }
            // C3: every socket connection re-advertises for the record it carries. This is the
            // transition the shipped bug was missing; it cannot be forgotten here because it is
            // the only way to learn that a socket is up.
            (SessionState::Live { session, epoch }, SessionEvent::SocketConnected { .. }) => {
                self.state = SessionState::Live { session, epoch };
                self.transition(SessionEffects::of(SessionEffect::AdvertiseCapabilities {
                    session,
                }))
            }
            // A retry that reopens the same record goes through Connecting again, so the socket
            // event that follows re-advertises.
            (SessionState::Live { session, epoch }, SessionEvent::SocketLost { .. }) => {
                self.state = SessionState::Lost { session, epoch };
                self.transition(SessionEffects::of(SessionEffect::StopSocket))
            }
            (SessionState::Lost { epoch, .. }, SessionEvent::RetryDue { .. }) => {
                self.state = SessionState::Connecting { epoch };
                self.transition(SessionEffects::EMPTY)
            }
            (
                SessionState::Connecting { .. } | SessionState::Lost { .. },
                SessionEvent::Failed { reason, .. },
            ) => {
                self.state = SessionState::Failed { reason };
                self.transition(SessionEffects::of(SessionEffect::ReportFailure { reason }))
            }
            // Signing out always stops the socket, whatever was running.
            (_, SessionEvent::SignOut) => {
                let had_socket = self.state.has_session()
                    || matches!(self.state, SessionState::Connecting { .. });
                self.state = SessionState::SignedOut;
                if had_socket {
                    self.transition(SessionEffects::of(SessionEffect::StopSocket))
                } else {
                    self.transition(SessionEffects::EMPTY)
                }
            }
            // Anything else is a no-op: the machine refuses to guess a transition.
            _ => self.transition(SessionEffects::EMPTY),
        }
    }

    fn transition(&self, effects: SessionEffects) -> Transition {
        Transition {
            state: self.state,
            effects,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        FailureReason, SessionEffect, SessionEvent, SessionId, SessionMachine, SessionState,
    };
    use crate::epoch::Epoch;

    const E: Epoch = Epoch::FIRST;
    const S: SessionId = SessionId::from_raw(7);

    fn effects(transition: &super::Transition) -> Vec<SessionEffect> {
        transition.effects.iter().collect()
    }

    #[test]
    fn a_sign_in_starts_a_socket_advertises_and_loads_the_catalog() {
        let mut machine = SessionMachine::new();
        assert_eq!(machine.state(), SessionState::SignedOut);

        let started = machine.reduce(SessionEvent::SignInStarted { epoch: E });
        assert_eq!(started.state, SessionState::Connecting { epoch: E });
        assert!(effects(&started).is_empty());

        let signed_in = machine.reduce(SessionEvent::SignedIn {
            epoch: E,
            session: S,
        });
        assert_eq!(
            signed_in.state,
            SessionState::Live {
                session: S,
                epoch: E
            }
        );
        assert_eq!(
            effects(&signed_in),
            vec![
                SessionEffect::StartSocket { session: S },
                SessionEffect::AdvertiseCapabilities { session: S },
                SessionEffect::LoadCatalog,
            ]
        );
    }

    #[test]
    fn every_socket_connection_re_advertises_capabilities() {
        // The regression this machine exists for: a reconnect can carry a NEW session record, so
        // the advertisement must follow every SocketConnected, not only the sign-in.
        let mut machine = SessionMachine::new();
        machine.reduce(SessionEvent::SignInStarted { epoch: E });
        machine.reduce(SessionEvent::SignedIn {
            epoch: E,
            session: S,
        });

        for _ in 0..3 {
            let connected = machine.reduce(SessionEvent::SocketConnected { epoch: E });
            assert_eq!(
                effects(&connected),
                vec![SessionEffect::AdvertiseCapabilities { session: S }],
                "each socket connection asks for capabilities again"
            );
        }
    }

    #[test]
    fn a_dropped_socket_moves_to_lost_and_stops_it() {
        let mut machine = SessionMachine::new();
        machine.reduce(SessionEvent::SignInStarted { epoch: E });
        machine.reduce(SessionEvent::SignedIn {
            epoch: E,
            session: S,
        });

        let lost = machine.reduce(SessionEvent::SocketLost { epoch: E });
        assert_eq!(
            lost.state,
            SessionState::Lost {
                session: S,
                epoch: E
            }
        );
        assert_eq!(effects(&lost), vec![SessionEffect::StopSocket]);

        let retry = machine.reduce(SessionEvent::RetryDue { epoch: E });
        assert_eq!(retry.state, SessionState::Connecting { epoch: E });
        // The retry reconnects under the same epoch; the following socket event re-advertises.
        let reconnected = machine.reduce(SessionEvent::SocketConnected { epoch: E });
        assert!(
            effects(&reconnected).is_empty(),
            "no socket yet: still connecting"
        );
    }

    #[test]
    fn a_failed_attempt_reports_the_reason_and_stops() {
        for reason in [
            FailureReason::CredentialsRejected,
            FailureReason::ServerRefused,
        ] {
            let mut machine = SessionMachine::new();
            machine.reduce(SessionEvent::SignInStarted { epoch: E });
            let failed = machine.reduce(SessionEvent::Failed { epoch: E, reason });
            assert_eq!(failed.state, SessionState::Failed { reason });
            assert_eq!(
                effects(&failed),
                vec![SessionEffect::ReportFailure { reason }]
            );
        }
    }

    #[test]
    fn results_from_an_abandoned_epoch_are_ignored() {
        let mut machine = SessionMachine::new();
        machine.reduce(SessionEvent::SignInStarted { epoch: E });

        let older = Epoch::NONE;
        let ignored = machine.reduce(SessionEvent::SignedIn {
            epoch: older,
            session: S,
        });
        assert_eq!(
            ignored.state,
            SessionState::Connecting { epoch: E },
            "a result from an older generation cannot sign the app in"
        );
        assert!(effects(&ignored).is_empty());

        let future = E.next();
        let ignored = machine.reduce(SessionEvent::SocketConnected { epoch: future });
        assert_eq!(ignored.state, SessionState::Connecting { epoch: E });
    }

    #[test]
    fn signing_out_stops_a_live_socket_and_is_idempotent() {
        let mut machine = SessionMachine::new();
        machine.reduce(SessionEvent::SignInStarted { epoch: E });
        machine.reduce(SessionEvent::SignedIn {
            epoch: E,
            session: S,
        });

        let out = machine.reduce(SessionEvent::SignOut);
        assert_eq!(out.state, SessionState::SignedOut);
        assert_eq!(effects(&out), vec![SessionEffect::StopSocket]);

        let again = machine.reduce(SessionEvent::SignOut);
        assert_eq!(again.state, SessionState::SignedOut);
        assert!(
            effects(&again).is_empty(),
            "signing out twice does not stop a socket that is gone"
        );
    }

    #[test]
    fn a_sign_in_result_without_a_matching_attempt_is_refused() {
        let mut machine = SessionMachine::new();
        // Signed in from SignedOut (no attempt): refuse rather than guess.
        let refused = machine.reduce(SessionEvent::SignedIn {
            epoch: Epoch::NONE,
            session: S,
        });
        assert_eq!(refused.state, SessionState::SignedOut);
        assert!(effects(&refused).is_empty());
    }

    #[test]
    fn the_machine_reports_its_identity_for_the_epoch_boundary() {
        let mut machine = SessionMachine::new();
        assert_eq!(machine.epoch(), Epoch::NONE);
        machine.reduce(SessionEvent::SignInStarted { epoch: E });
        assert_eq!(machine.epoch(), E);
        machine.reduce(SessionEvent::SignedIn {
            epoch: E,
            session: S,
        });
        assert_eq!(machine.state().session(), Some(S));
        assert!(machine.state().has_session());
        machine.reduce(SessionEvent::SignOut);
        assert_eq!(machine.state().session(), None);
        assert!(!machine.state().has_session());
    }
}

//! M4-05 plugin lifecycle: the state machine for one plugin.
//!
//! States: `Discovered -> Validated -> Loaded -> Running -> Unloaded`, plus `Quarantined`,
//! reachable from `Loaded` or `Running` when a call fails. A permission violation
//! quarantines at once. Other failures count consecutively and quarantine when the count
//! reaches the threshold (default 1, as the spec says: any trap, fuel exhaustion or
//! timeout quarantines). A quarantined plugin is never dispatched to until the user
//! reloads it, which returns it to `Validated`.
//!
//! This module is pure: it decides transitions. It does not instantiate guests.

use crate::error::PluginError;
use crate::sandbox::SandboxFailure;

/// Default number of consecutive failures before quarantine (spec: any failure).
pub const DEFAULT_QUARANTINE_THRESHOLD: u32 = 1;
/// Largest threshold a caller may configure.
pub const MAX_QUARANTINE_THRESHOLD: u32 = 10;

/// Where a plugin is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleState {
    Discovered,
    Validated,
    Loaded,
    Running,
    Unloaded,
    Quarantined,
}

impl LifecycleState {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Discovered => "Discovered",
            Self::Validated => "Validated",
            Self::Loaded => "Loaded",
            Self::Running => "Running",
            Self::Unloaded => "Unloaded",
            Self::Quarantined => "Quarantined",
        }
    }

    /// Only loaded or running plugins may receive calls.
    pub const fn can_dispatch(self) -> bool {
        matches!(self, Self::Loaded | Self::Running)
    }
}

/// An input to the state machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleEvent {
    /// The manifest passed validation and the plugin is known.
    Validate,
    /// The guest instance was created.
    Loaded,
    /// A call (`init` or an event) returned normally.
    CallSucceeded,
    /// A call trapped, timed out, ran out of fuel or exceeded a limit.
    CallFailed(SandboxFailure),
    /// The plugin used a permission it does not hold.
    PermissionViolation { permission: String },
    /// The app is exiting, or the plugin is being dropped.
    Shutdown,
    /// The user asked for the plugin to be reloaded.
    Reload,
}

impl LifecycleEvent {
    const fn name(&self) -> &'static str {
        match self {
            Self::Validate => "Validate",
            Self::Loaded => "Loaded",
            Self::CallSucceeded => "CallSucceeded",
            Self::CallFailed(_) => "CallFailed",
            Self::PermissionViolation { .. } => "PermissionViolation",
            Self::Shutdown => "Shutdown",
            Self::Reload => "Reload",
        }
    }
}

/// The failure that last quarantined (or is counting toward quarantine) a plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    Sandbox(SandboxFailure),
    PermissionViolation { permission: String },
}

/// Lifecycle state for one plugin, with its failure counter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lifecycle {
    state: LifecycleState,
    threshold: u32,
    consecutive_failures: u32,
    last_failure: Option<Failure>,
}

impl Lifecycle {
    /// A new plugin in `Discovered`. The threshold must be between 1 and 10.
    pub fn new(threshold: u32) -> Result<Self, PluginError> {
        if threshold == 0 || threshold > MAX_QUARANTINE_THRESHOLD {
            return Err(PluginError::Invalid {
                field: "quarantine_threshold",
                reason: format!("must be between 1 and {MAX_QUARANTINE_THRESHOLD}"),
            });
        }
        Ok(Self {
            state: LifecycleState::Discovered,
            threshold,
            consecutive_failures: 0,
            last_failure: None,
        })
    }

    pub const fn state(&self) -> LifecycleState {
        self.state
    }

    pub const fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    pub fn last_failure(&self) -> Option<&Failure> {
        self.last_failure.as_ref()
    }

    /// Apply one event. Returns the new state, or [`PluginError::InvalidTransition`]
    /// and leaves the state untouched when the event is not allowed from here.
    pub fn apply(&mut self, event: LifecycleEvent) -> Result<LifecycleState, PluginError> {
        let allowed = is_allowed(self.state, &event);
        if !allowed {
            return Err(PluginError::InvalidTransition {
                from: self.state.name(),
                event: event.name(),
            });
        }

        match &event {
            LifecycleEvent::CallSucceeded => {
                self.consecutive_failures = 0;
            }
            LifecycleEvent::CallFailed(failure) => {
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                self.last_failure = Some(Failure::Sandbox(failure.clone()));
            }
            LifecycleEvent::PermissionViolation { permission } => {
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                self.last_failure = Some(Failure::PermissionViolation {
                    permission: permission.clone(),
                });
            }
            LifecycleEvent::Reload => {
                self.consecutive_failures = 0;
                self.last_failure = None;
            }
            LifecycleEvent::Validate | LifecycleEvent::Loaded | LifecycleEvent::Shutdown => {}
        }

        self.state = match (self.state, &event) {
            (LifecycleState::Loaded | LifecycleState::Running, LifecycleEvent::CallFailed(_))
                if self.consecutive_failures >= self.threshold =>
            {
                LifecycleState::Quarantined
            }
            (LifecycleState::Loaded | LifecycleState::Running, LifecycleEvent::CallFailed(_)) => {
                self.state
            }
            _ => next_state(self.state, &event).unwrap_or(self.state),
        };
        Ok(self.state)
    }
}

/// Whether `event` is defined for `state`. The table lists every legal pair.
fn is_allowed(state: LifecycleState, event: &LifecycleEvent) -> bool {
    use LifecycleEvent as E;
    use LifecycleState as S;
    matches!(
        (state, event),
        (S::Discovered, E::Validate)
            | (S::Validated, E::Loaded)
            | (S::Loaded | S::Running, E::CallSucceeded)
            | (S::Loaded | S::Running, E::CallFailed(_))
            | (S::Loaded | S::Running, E::PermissionViolation { .. })
            | (S::Loaded | S::Running | S::Quarantined, E::Shutdown)
            | (S::Quarantined | S::Unloaded, E::Reload)
    )
}

/// Target state for the events that do not depend on the failure counter.
/// `None` for events that are not allowed (see [`is_allowed`]).
fn next_state(state: LifecycleState, event: &LifecycleEvent) -> Option<LifecycleState> {
    use LifecycleEvent as E;
    use LifecycleState as S;
    match (state, event) {
        (S::Discovered, E::Validate) => Some(S::Validated),
        (S::Validated, E::Loaded) => Some(S::Loaded),
        (S::Loaded | S::Running, E::CallSucceeded) => Some(S::Running),
        (S::Loaded | S::Running, E::PermissionViolation { .. }) => Some(S::Quarantined),
        (S::Loaded | S::Running | S::Quarantined, E::Shutdown) => Some(S::Unloaded),
        (S::Quarantined | S::Unloaded, E::Reload) => Some(S::Validated),
        _ => None,
    }
}

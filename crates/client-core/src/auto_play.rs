//! When a one-click Play may start the stream.
//!
//! `Play` on an item first needs the item record and the playback options, both fetched by the
//! API worker. The UI polls every frame and asks [`decide`] what to do with the pending request.
//! The decision is pure: the UI gathers the state, calls this, and acts on the answer.
//!
//! [`AutoPlayMachine`] owns the pending request itself, so the UI no longer keeps the request in a
//! loose `Option` next to the loading flags it reads. The machine holds the request, calls
//! [`decide`] on each poll, and returns the effect to run. It is pure: no I/O, no clock.

use crate::playback_plan::{PlaybackMethod, PlaybackSelection};

/// Whether the playback options (the server's `PlaybackInfo`) can be used yet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OptionsState {
    /// Not received yet.
    NotLoaded,
    /// Received but unusable: the server reported an error code, or it listed no media source.
    Unusable,
    /// Received with at least one source. `pick` is the source the client can play, if any.
    Ready { pick: Option<PlaybackSelection> },
}

/// Whether the item record (for the resume position) is available.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DetailsState {
    /// Not there; `loading` says whether a request is still in flight.
    Missing { loading: bool },
    /// There. `resume_ticks` is the saved position when the item is partly watched.
    Loaded { resume_ticks: Option<i64> },
}

/// Everything the decision depends on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AutoPlayInput {
    /// The item on screen is still the one Play was pressed for.
    pub item_unchanged: bool,
    /// The options request failed.
    pub options_failed: bool,
    /// The item-record request failed.
    pub details_failed: bool,
    /// A start request for this item is already running.
    pub start_in_flight: bool,
    pub options: OptionsState,
    pub details: DetailsState,
    /// Play was pressed as "start over": ignore the saved position.
    pub start_over: bool,
    /// Start exactly here when set, from a remote "play now" request. It wins over the saved
    /// position and over `start_over`, because the requesting client chose this position.
    pub requested_ticks: Option<i64>,
}

/// What the UI does with the pending request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AutoPlayDecision {
    /// Keep the request and look again next frame.
    Wait,
    /// Forget the request. The UI already shows why, or the viewer moved on.
    Drop,
    /// Forget the request and tell the viewer no source on this server can be played here.
    DropNoCompatibleSource,
    /// Start the stream.
    Start {
        source_index: usize,
        method: PlaybackMethod,
        /// Where to begin, in 100 ns ticks. `None` starts at the beginning.
        resume_ticks: Option<i64>,
    },
}

/// Decides what to do with a pending one-click Play. The checks run in a fixed order, the same
/// order the tests below pin down.
pub fn decide(input: &AutoPlayInput) -> AutoPlayDecision {
    if !input.item_unchanged || input.options_failed || input.details_failed {
        return AutoPlayDecision::Drop;
    }
    if input.start_in_flight {
        return AutoPlayDecision::Wait;
    }
    let pick = match input.options {
        OptionsState::NotLoaded => return AutoPlayDecision::Wait,
        OptionsState::Unusable => return AutoPlayDecision::Drop,
        OptionsState::Ready { pick } => pick,
    };
    let resume_ticks = match input.details {
        DetailsState::Missing { loading: true } => return AutoPlayDecision::Wait,
        DetailsState::Missing { loading: false } => return AutoPlayDecision::Drop,
        DetailsState::Loaded { resume_ticks } => resume_ticks,
    };
    let Some(pick) = pick else {
        return AutoPlayDecision::DropNoCompatibleSource;
    };
    let resume_ticks = match input.requested_ticks {
        Some(ticks) => Some(ticks.max(0)),
        None if input.start_over => None,
        None => resume_ticks,
    };
    AutoPlayDecision::Start {
        source_index: pick.source_index,
        method: pick.method,
        resume_ticks,
    }
}

/// A one-click play request waiting for the item record and the playback options.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayRequest {
    /// The item Play was pressed for.
    pub item_id: String,
    /// Play was pressed as "start over": ignore the saved position.
    pub start_over: bool,
    /// Start exactly here when set, from a remote "play now" request.
    pub requested_ticks: Option<i64>,
}

/// What the UI observed this frame, gathered from its own state.
///
/// These are the inputs to [`decide`] that the UI owns. The request-specific inputs (`start_over`,
/// `requested_ticks`) belong to the machine and are not repeated here.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AutoPlayObservations {
    /// The item on screen is still the one Play was pressed for.
    pub item_unchanged: bool,
    /// The options request failed.
    pub options_failed: bool,
    /// The item-record request failed.
    pub details_failed: bool,
    /// A start request for this item is already running.
    pub start_in_flight: bool,
    pub options: OptionsState,
    pub details: DetailsState,
}

/// Work the UI must do after a poll. Declared, not executed by the machine.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AutoPlayEffect {
    /// Start the stream at `resume_ticks`.
    Start {
        source_index: usize,
        method: PlaybackMethod,
        resume_ticks: Option<i64>,
    },
    /// Tell the viewer no source on this server can be played here.
    NoCompatibleSource,
}

/// The result of one poll.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AutoPlayOutcome {
    /// No request is pending.
    Idle,
    /// A request is pending and must keep waiting.
    Waiting,
    /// The request finished and was cleared. `effect` is what the UI must do, if anything.
    Finished { effect: Option<AutoPlayEffect> },
}

/// Owns the pending one-click play request and decides, per poll, whether to keep waiting or to
/// start. Pure: the caller supplies observations and runs the returned effect.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AutoPlayMachine {
    pending: Option<PlayRequest>,
}

impl AutoPlayMachine {
    pub const fn new() -> Self {
        Self { pending: None }
    }

    /// Record a new request. A second request replaces the first: Play is an intent, and the
    /// newest one wins.
    pub fn request(&mut self, request: PlayRequest) {
        self.pending = Some(request);
    }

    /// Forget the pending request, if any. Idempotent.
    pub fn cancel(&mut self) {
        self.pending = None;
    }

    /// Whether a request is pending.
    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// The pending request, for logging and tests.
    pub fn pending(&self) -> Option<&PlayRequest> {
        self.pending.as_ref()
    }

    /// Whether the pending request is for `item_id`. `false` when nothing is pending.
    pub fn is_pending_for(&self, item_id: &str) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|request| request.item_id == item_id)
    }

    /// Advance by one poll. Returns the outcome and, when the request finished, the effect to run.
    ///
    /// The decision itself is [`decide`]; this only supplies the request-owned inputs and clears
    /// the request once it is consumed, so the ordering rules live in one place.
    pub fn reduce(&mut self, observations: &AutoPlayObservations) -> AutoPlayOutcome {
        let Some(request) = self.pending.as_ref() else {
            return AutoPlayOutcome::Idle;
        };
        let decision = decide(&AutoPlayInput {
            item_unchanged: observations.item_unchanged,
            options_failed: observations.options_failed,
            details_failed: observations.details_failed,
            start_in_flight: observations.start_in_flight,
            options: observations.options,
            details: observations.details,
            start_over: request.start_over,
            requested_ticks: request.requested_ticks,
        });
        match decision {
            AutoPlayDecision::Wait => AutoPlayOutcome::Waiting,
            AutoPlayDecision::Drop => {
                self.pending = None;
                AutoPlayOutcome::Finished { effect: None }
            }
            AutoPlayDecision::DropNoCompatibleSource => {
                self.pending = None;
                AutoPlayOutcome::Finished {
                    effect: Some(AutoPlayEffect::NoCompatibleSource),
                }
            }
            AutoPlayDecision::Start {
                source_index,
                method,
                resume_ticks,
            } => {
                self.pending = None;
                AutoPlayOutcome::Finished {
                    effect: Some(AutoPlayEffect::Start {
                        source_index,
                        method,
                        resume_ticks,
                    }),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PICK: PlaybackSelection = PlaybackSelection {
        source_index: 1,
        method: PlaybackMethod::DirectPlay,
    };

    fn ready() -> AutoPlayInput {
        AutoPlayInput {
            item_unchanged: true,
            options_failed: false,
            details_failed: false,
            start_in_flight: false,
            options: OptionsState::Ready { pick: Some(PICK) },
            details: DetailsState::Loaded {
                resume_ticks: Some(600_000_000),
            },
            start_over: false,
            requested_ticks: None,
        }
    }

    #[test]
    fn everything_loaded_starts_at_the_saved_position() {
        assert_eq!(
            decide(&ready()),
            AutoPlayDecision::Start {
                source_index: 1,
                method: PlaybackMethod::DirectPlay,
                resume_ticks: Some(600_000_000),
            }
        );
    }

    #[test]
    fn start_over_ignores_the_saved_position() {
        let input = AutoPlayInput {
            start_over: true,
            ..ready()
        };
        assert!(matches!(
            decide(&input),
            AutoPlayDecision::Start {
                resume_ticks: None,
                ..
            }
        ));
    }

    #[test]
    fn an_unwatched_item_starts_at_the_beginning() {
        let input = AutoPlayInput {
            details: DetailsState::Loaded { resume_ticks: None },
            ..ready()
        };
        assert!(matches!(
            decide(&input),
            AutoPlayDecision::Start {
                resume_ticks: None,
                ..
            }
        ));
    }

    #[test]
    fn the_method_and_source_come_from_the_pick() {
        for (index, method) in [
            (0, PlaybackMethod::DirectPlay),
            (2, PlaybackMethod::DirectStream),
            (3, PlaybackMethod::Transcode),
        ] {
            let input = AutoPlayInput {
                options: OptionsState::Ready {
                    pick: Some(PlaybackSelection {
                        source_index: index,
                        method,
                    }),
                },
                ..ready()
            };
            assert!(matches!(
                decide(&input),
                AutoPlayDecision::Start { source_index, method: m, .. }
                    if source_index == index && m == method
            ));
        }
    }

    #[test]
    fn a_different_item_on_screen_drops_the_request() {
        let input = AutoPlayInput {
            item_unchanged: false,
            ..ready()
        };
        assert_eq!(decide(&input), AutoPlayDecision::Drop);
    }

    #[test]
    fn a_negotiation_or_details_error_drops_the_request() {
        for input in [
            AutoPlayInput {
                options_failed: true,
                ..ready()
            },
            AutoPlayInput {
                details_failed: true,
                ..ready()
            },
        ] {
            assert_eq!(decide(&input), AutoPlayDecision::Drop);
        }
    }

    #[test]
    fn a_start_already_running_waits_even_when_everything_else_is_ready() {
        let input = AutoPlayInput {
            start_in_flight: true,
            ..ready()
        };
        assert_eq!(decide(&input), AutoPlayDecision::Wait);
    }

    #[test]
    fn a_requested_position_wins_over_the_saved_position_and_start_over() {
        let remote = AutoPlayInput {
            requested_ticks: Some(120_000_000),
            ..ready()
        };
        assert_eq!(
            decide(&remote),
            AutoPlayDecision::Start {
                source_index: 1,
                method: PlaybackMethod::DirectPlay,
                resume_ticks: Some(120_000_000),
            }
        );
        let remote_with_start_over = AutoPlayInput {
            requested_ticks: Some(0),
            start_over: true,
            ..ready()
        };
        assert!(matches!(
            decide(&remote_with_start_over),
            AutoPlayDecision::Start {
                resume_ticks: Some(0),
                ..
            }
        ));
        let negative = AutoPlayInput {
            requested_ticks: Some(-10),
            ..ready()
        };
        assert!(matches!(
            decide(&negative),
            AutoPlayDecision::Start {
                resume_ticks: Some(0),
                ..
            }
        ));
    }

    #[test]
    fn a_changed_item_or_error_wins_over_a_running_start() {
        let input = AutoPlayInput {
            start_in_flight: true,
            item_unchanged: false,
            ..ready()
        };
        assert_eq!(decide(&input), AutoPlayDecision::Drop);
    }

    #[test]
    fn options_not_loaded_waits() {
        let input = AutoPlayInput {
            options: OptionsState::NotLoaded,
            ..ready()
        };
        assert_eq!(decide(&input), AutoPlayDecision::Wait);
    }

    #[test]
    fn unusable_options_drop_before_details_are_considered() {
        let input = AutoPlayInput {
            options: OptionsState::Unusable,
            details: DetailsState::Missing { loading: true },
            ..ready()
        };
        assert_eq!(decide(&input), AutoPlayDecision::Drop);
    }

    #[test]
    fn details_still_loading_waits_and_details_missing_without_a_request_drops() {
        let loading = AutoPlayInput {
            details: DetailsState::Missing { loading: true },
            ..ready()
        };
        assert_eq!(decide(&loading), AutoPlayDecision::Wait);
        let gone = AutoPlayInput {
            details: DetailsState::Missing { loading: false },
            ..ready()
        };
        assert_eq!(decide(&gone), AutoPlayDecision::Drop);
    }

    #[test]
    fn no_compatible_source_drops_with_a_message() {
        let input = AutoPlayInput {
            options: OptionsState::Ready { pick: None },
            ..ready()
        };
        assert_eq!(decide(&input), AutoPlayDecision::DropNoCompatibleSource);
    }

    #[test]
    fn missing_details_are_reported_before_a_missing_source() {
        // With no item record there is no resume position to decide on yet, so the request
        // waits instead of reporting an error that a later reply could make moot.
        let input = AutoPlayInput {
            options: OptionsState::Ready { pick: None },
            details: DetailsState::Missing { loading: true },
            ..ready()
        };
        assert_eq!(decide(&input), AutoPlayDecision::Wait);
    }

    fn request(item_id: &str) -> PlayRequest {
        PlayRequest {
            item_id: item_id.to_owned(),
            start_over: false,
            requested_ticks: None,
        }
    }

    fn observations() -> AutoPlayObservations {
        let input = ready();
        AutoPlayObservations {
            item_unchanged: input.item_unchanged,
            options_failed: input.options_failed,
            details_failed: input.details_failed,
            start_in_flight: input.start_in_flight,
            options: input.options,
            details: input.details,
        }
    }

    #[test]
    fn a_machine_with_no_request_is_idle() {
        let mut machine = AutoPlayMachine::new();
        assert!(!machine.is_pending());
        assert_eq!(machine.reduce(&observations()), AutoPlayOutcome::Idle);
        assert_eq!(machine.pending(), None);
    }

    #[test]
    fn a_pending_request_waits_until_both_replies_are_usable() {
        let mut machine = AutoPlayMachine::new();
        machine.request(request("movie"));
        assert!(machine.is_pending());
        assert!(machine.is_pending_for("movie"));

        let waiting = AutoPlayObservations {
            options: OptionsState::NotLoaded,
            ..observations()
        };
        assert_eq!(machine.reduce(&waiting), AutoPlayOutcome::Waiting);
        assert!(machine.is_pending(), "still pending");

        let loading = AutoPlayObservations {
            details: DetailsState::Missing { loading: true },
            ..observations()
        };
        assert_eq!(machine.reduce(&loading), AutoPlayOutcome::Waiting);
        assert!(machine.is_pending());
    }

    #[test]
    fn a_ready_request_starts_once_and_clears_itself() {
        let mut machine = AutoPlayMachine::new();
        machine.request(request("movie"));
        assert_eq!(
            machine.reduce(&observations()),
            AutoPlayOutcome::Finished {
                effect: Some(AutoPlayEffect::Start {
                    source_index: 1,
                    method: PlaybackMethod::DirectPlay,
                    resume_ticks: Some(600_000_000),
                })
            }
        );
        assert!(!machine.is_pending(), "the start consumes the request");
        assert_eq!(machine.reduce(&observations()), AutoPlayOutcome::Idle);
    }

    #[test]
    fn the_request_owned_inputs_reach_the_decision() {
        let mut start_over = AutoPlayMachine::new();
        start_over.request(PlayRequest {
            start_over: true,
            ..request("movie")
        });
        assert!(matches!(
            start_over.reduce(&observations()),
            AutoPlayOutcome::Finished {
                effect: Some(AutoPlayEffect::Start {
                    resume_ticks: None,
                    ..
                })
            }
        ));

        let mut remote = AutoPlayMachine::new();
        remote.request(PlayRequest {
            requested_ticks: Some(120_000_000),
            ..request("movie")
        });
        assert!(matches!(
            remote.reduce(&observations()),
            AutoPlayOutcome::Finished {
                effect: Some(AutoPlayEffect::Start {
                    resume_ticks: Some(120_000_000),
                    ..
                })
            }
        ));
    }

    #[test]
    fn moving_to_another_item_drops_the_request_without_an_effect() {
        let mut machine = AutoPlayMachine::new();
        machine.request(request("movie"));
        let moved = AutoPlayObservations {
            item_unchanged: false,
            ..observations()
        };
        assert_eq!(
            machine.reduce(&moved),
            AutoPlayOutcome::Finished { effect: None }
        );
        assert!(!machine.is_pending());
    }

    #[test]
    fn unusable_options_and_errors_drop_without_an_effect() {
        for changed in [
            AutoPlayObservations {
                options_failed: true,
                ..observations()
            },
            AutoPlayObservations {
                details_failed: true,
                ..observations()
            },
            AutoPlayObservations {
                options: OptionsState::Unusable,
                ..observations()
            },
        ] {
            let mut machine = AutoPlayMachine::new();
            machine.request(request("movie"));
            assert_eq!(
                machine.reduce(&changed),
                AutoPlayOutcome::Finished { effect: None }
            );
            assert!(!machine.is_pending());
        }
    }

    #[test]
    fn no_compatible_source_clears_the_request_and_reports_it() {
        let mut machine = AutoPlayMachine::new();
        machine.request(request("movie"));
        let none = AutoPlayObservations {
            options: OptionsState::Ready { pick: None },
            ..observations()
        };
        assert_eq!(
            machine.reduce(&none),
            AutoPlayOutcome::Finished {
                effect: Some(AutoPlayEffect::NoCompatibleSource)
            }
        );
        assert!(!machine.is_pending());
    }

    #[test]
    fn a_start_already_in_flight_keeps_waiting() {
        let mut machine = AutoPlayMachine::new();
        machine.request(request("movie"));
        let in_flight = AutoPlayObservations {
            start_in_flight: true,
            ..observations()
        };
        assert_eq!(machine.reduce(&in_flight), AutoPlayOutcome::Waiting);
        assert!(machine.is_pending(), "do not start twice");
    }

    #[test]
    fn a_second_request_replaces_the_first_and_cancel_is_idempotent() {
        let mut machine = AutoPlayMachine::new();
        machine.request(request("first"));
        machine.request(request("second"));
        assert!(machine.is_pending_for("second"));
        assert!(!machine.is_pending_for("first"));

        machine.cancel();
        machine.cancel();
        assert!(!machine.is_pending());
        assert_eq!(machine.reduce(&observations()), AutoPlayOutcome::Idle);
    }

    #[test]
    fn a_stale_poll_for_the_old_item_cannot_start_the_new_request() {
        // The newest intent wins: a poll whose observations still describe the previous item
        // drops the request rather than starting the wrong one.
        let mut machine = AutoPlayMachine::new();
        machine.request(request("new-item"));
        let stale = AutoPlayObservations {
            item_unchanged: false,
            ..observations()
        };
        assert_eq!(
            machine.reduce(&stale),
            AutoPlayOutcome::Finished { effect: None }
        );
        assert!(!machine.is_pending());
    }
}

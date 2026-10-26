//! When a one-click Play may start the stream.
//!
//! `Play` on an item first needs the item record and the playback options, both fetched by the
//! API worker. The UI polls every frame and asks [`decide`] what to do with the pending request.
//! The decision is pure: the UI gathers the state, calls this, and acts on the answer.

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
    AutoPlayDecision::Start {
        source_index: pick.source_index,
        method: pick.method,
        resume_ticks: if input.start_over { None } else { resume_ticks },
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
}

//! Pure presentation rules for the Live TV channel list.

use jellyfin_api::LiveTvChannel;
use std::cmp::Ordering;
use std::collections::HashSet;

/// Stable channel ordering: favorites first, then numbered channels by number,
/// then unnumbered channels; names and IDs break ties deterministically.
pub fn sort_channels(channels: &mut [LiveTvChannel], favorites: &HashSet<String>) {
    channels.sort_by(|left, right| {
        let left_favorite = favorites.contains(&left.item.id);
        let right_favorite = favorites.contains(&right.item.id);
        right_favorite
            .cmp(&left_favorite)
            .then_with(|| {
                compare_channel_numbers(
                    left.number.as_deref().or(left.channel_number.as_deref()),
                    right.number.as_deref().or(right.channel_number.as_deref()),
                )
            })
            .then_with(|| left.item.name.cmp(&right.item.name))
            .then_with(|| left.item.id.cmp(&right.item.id))
    });
}

/// A missing or empty channel lineup is a normal server capability state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChannelListState {
    Ready,
    NotSetUp,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum TunePhase {
    Opening,
    Negotiating,
}

struct TuneAttempt {
    generation: u64,
    channel_id: String,
    live_stream_id: Option<String>,
    phase: TunePhase,
}

/// Effects emitted by [`LiveTuneCoordinator`]. Live-stream identifiers are
/// deliberately not printable through `Debug` and must never be logged.
pub enum LiveTuneEffect {
    Open {
        generation: u64,
        channel_id: String,
    },
    Negotiate {
        generation: u64,
        channel_id: String,
        live_stream_id: String,
    },
    Close {
        live_stream_id: String,
    },
    Start {
        generation: u64,
        channel_id: String,
        live_stream_id: String,
    },
}

/// Tracks one Live TV tune attempt and closes streams returned by stale opens.
/// A generation token rejects late responses after a channel switch; if an
/// in-flight open completes after that switch, its newly opened stream is
/// emitted as a `Close` effect instead of being orphaned.
#[derive(Default)]
pub struct LiveTuneCoordinator {
    generation: u64,
    current: Option<TuneAttempt>,
}

impl LiveTuneCoordinator {
    pub fn begin(&mut self, channel_id: String) -> Vec<LiveTuneEffect> {
        let mut effects = Vec::with_capacity(2);
        if let Some(previous) = self.current.take()
            && let Some(live_stream_id) = previous.live_stream_id
        {
            effects.push(LiveTuneEffect::Close { live_stream_id });
        }
        self.generation = self.generation.wrapping_add(1).max(1);
        self.current = Some(TuneAttempt {
            generation: self.generation,
            channel_id: channel_id.clone(),
            live_stream_id: None,
            phase: TunePhase::Opening,
        });
        effects.push(LiveTuneEffect::Open {
            generation: self.generation,
            channel_id,
        });
        effects
    }

    pub fn opened(&mut self, generation: u64, live_stream_id: String) -> Vec<LiveTuneEffect> {
        let Some(attempt) = self.current.as_mut().filter(|attempt| {
            attempt.generation == generation && attempt.phase == TunePhase::Opening
        }) else {
            return vec![LiveTuneEffect::Close { live_stream_id }];
        };
        attempt.live_stream_id = Some(live_stream_id.clone());
        attempt.phase = TunePhase::Negotiating;
        vec![LiveTuneEffect::Negotiate {
            generation,
            channel_id: attempt.channel_id.clone(),
            live_stream_id,
        }]
    }

    pub fn prepared(&self, generation: u64) -> Option<LiveTuneEffect> {
        let attempt = self.current.as_ref().filter(|attempt| {
            attempt.generation == generation && attempt.phase == TunePhase::Negotiating
        })?;
        Some(LiveTuneEffect::Start {
            generation,
            channel_id: attempt.channel_id.clone(),
            live_stream_id: attempt.live_stream_id.clone()?,
        })
    }

    pub fn stop(&mut self) -> Option<LiveTuneEffect> {
        let attempt = self.current.take()?;
        attempt
            .live_stream_id
            .map(|live_stream_id| LiveTuneEffect::Close { live_stream_id })
    }

    pub fn is_current(&self, generation: u64) -> bool {
        self.current
            .as_ref()
            .is_some_and(|attempt| attempt.generation == generation)
    }
}

pub fn channel_list_state(
    channels: &[LiveTvChannel],
    endpoint_available: bool,
) -> ChannelListState {
    if endpoint_available && !channels.is_empty() {
        ChannelListState::Ready
    } else {
        ChannelListState::NotSetUp
    }
}

fn compare_channel_numbers(left: Option<&str>, right: Option<&str>) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => match (left.parse::<f64>(), right.parse::<f64>()) {
            (Ok(left_number), Ok(right_number)) => left_number
                .total_cmp(&right_number)
                .then_with(|| left.cmp(right)),
            _ => left.cmp(right),
        },
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ChannelListState, LiveTuneCoordinator, LiveTuneEffect, channel_list_state, sort_channels,
    };
    use jellyfin_api::LiveTvChannel;
    use std::collections::HashSet;

    fn channel(id: &str, name: &str, number: Option<&str>) -> LiveTvChannel {
        LiveTvChannel {
            item: serde_json::from_value(serde_json::json!({"Id": id, "Name": name}))
                .expect("valid channel item"),
            channel_number: None,
            number: number.map(str::to_owned),
            channel_type: None,
            current_program: None,
        }
    }

    #[test]
    fn channels_sort_by_number_then_name_with_missing_numbers_last() {
        let mut channels = vec![
            channel("u", "Zulu", None),
            channel("b", "Beta", Some("2")),
            channel("a", "Alpha", Some("2")),
            channel("c", "One", Some("1")),
        ];
        sort_channels(&mut channels, &HashSet::new());
        let ids: Vec<_> = channels
            .iter()
            .map(|channel| channel.item.id.as_str())
            .collect();
        assert_eq!(ids, ["c", "a", "b", "u"]);
    }

    #[test]
    fn favorites_sort_first_without_losing_channel_number_order() {
        let mut channels = vec![
            channel("normal", "Normal", Some("1")),
            channel("fav-late", "Favorite later", Some("8")),
            channel("fav-early", "Favorite early", Some("2")),
        ];
        let favorites = HashSet::from(["fav-late".to_owned(), "fav-early".to_owned()]);
        sort_channels(&mut channels, &favorites);
        let ids: Vec<_> = channels
            .iter()
            .map(|channel| channel.item.id.as_str())
            .collect();
        assert_eq!(ids, ["fav-early", "fav-late", "normal"]);
    }

    #[test]
    fn an_unavailable_or_empty_lineup_uses_the_setup_state() {
        assert_eq!(channel_list_state(&[], true), ChannelListState::NotSetUp);
        assert_eq!(channel_list_state(&[], false), ChannelListState::NotSetUp);
        let channels = [channel("one", "One", Some("1"))];
        assert_eq!(channel_list_state(&channels, true), ChannelListState::Ready);
        assert_eq!(
            channel_list_state(&channels, false),
            ChannelListState::NotSetUp
        );
    }

    #[test]
    fn a_late_open_after_channel_switch_is_closed_not_prepared() {
        let mut tune = LiveTuneCoordinator::default();
        let first = tune.begin("channel-one".into());
        let [
            LiveTuneEffect::Open {
                generation: first_generation,
                ..
            },
        ] = first.as_slice()
        else {
            panic!("first tune should request an open");
        };
        let second = tune.begin("channel-two".into());
        let [
            LiveTuneEffect::Open {
                generation: second_generation,
                ..
            },
        ] = second.as_slice()
        else {
            panic!("switch should start the new channel");
        };
        assert!(!tune.is_current(*first_generation));
        assert!(tune.is_current(*second_generation));

        let stale_result = tune.opened(*first_generation, "orphaned-live-id".into());
        assert!(matches!(
            stale_result.as_slice(),
            [LiveTuneEffect::Close { live_stream_id }] if live_stream_id == "orphaned-live-id"
        ));
        assert!(tune.prepared(*first_generation).is_none());
    }

    #[test]
    fn current_open_negotiates_then_stop_closes_the_live_stream() {
        let mut tune = LiveTuneCoordinator::default();
        let open = tune.begin("channel-one".into());
        let [LiveTuneEffect::Open { generation, .. }] = open.as_slice() else {
            panic!("tune should request an open");
        };
        let negotiated = tune.opened(*generation, "live-id".into());
        assert!(matches!(
            negotiated.as_slice(),
            [LiveTuneEffect::Negotiate { live_stream_id, .. }] if live_stream_id == "live-id"
        ));
        assert!(matches!(
            tune.prepared(*generation),
            Some(LiveTuneEffect::Start { live_stream_id, .. }) if live_stream_id == "live-id"
        ));
        assert!(matches!(
            tune.stop(),
            Some(LiveTuneEffect::Close { live_stream_id }) if live_stream_id == "live-id"
        ));
        assert!(!tune.is_current(*generation));
    }
}

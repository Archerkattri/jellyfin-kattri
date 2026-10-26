//! Playback event bus: fan-out of read-only playback notices to subscribers.
//!
//! Std only. The bus is owned by the playback owner, which calls [`EventBus::publish`] or
//! [`EventBus::publish_at`]. Each subscriber gets a bounded queue through a [`Subscription`]
//! and may read it from any thread.
//!
//! Delivery rules:
//! - Publish never waits. A subscriber's queue lock is held only while the queue is changed.
//! - Progress events ([`PlaybackBusEvent::Position`] and [`PlaybackBusEvent::Snapshot`]) are
//!   throttled bus-wide to one accepted event per [`PROGRESS_THROTTLE`]. A throttled event
//!   is not delivered to anyone and is counted in [`EventBus::throttled_total`].
//! - Start and stop events ([`PlaybackBusEvent::Started`], [`PlaybackBusEvent::Stopped`]) are
//!   never dropped. A full queue makes room for them by evicting the oldest progress event,
//!   then the oldest other event. If the queue holds only start and stop events, the new one
//!   is kept past the nominal capacity.
//! - Any other event that finds a full queue evicts the oldest progress event, or is dropped
//!   if the queue holds none.
//! - Each loss is counted per subscriber and in total.
//! - A quarantined subscriber gets nothing new queued. Events published while it is
//!   quarantined are counted in [`Subscription::quarantine_dropped`], not in the overflow
//!   counters, so the two causes stay distinguishable.
//! - A subscriber that joins mid-playback starts with one [`PlaybackBusEvent::Snapshot`] of
//!   the current state. The state follows every publish, including throttled progress.
//! - Error text that could carry a URL or a credential is replaced before it is queued.
//!
//! This type is separate from the playback state-machine `PlaybackEvent` at the crate root.
//! It is the outward notice stream, not the state input.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Minimum time between two accepted progress events, bus-wide.
pub const PROGRESS_THROTTLE: Duration = Duration::from_secs(5);

/// A read-only notice about playback. Payloads carry no stream URLs, tokens or server origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaybackBusEvent {
    Started,
    Position {
        position_ms: u64,
    },
    /// Full playback state at a point in the middle of an item. Progress-class: throttled
    /// and evictable like [`PlaybackBusEvent::Position`].
    Snapshot {
        position_ms: u64,
        duration_ms: Option<u64>,
        paused: bool,
    },
    Paused,
    Resumed,
    Stopped,
    TrackChanged {
        index: usize,
    },
    Error {
        message: String,
    },
}

impl PlaybackBusEvent {
    /// Progress-class events are throttled and are the first evicted from a full queue.
    fn is_progress(&self) -> bool {
        matches!(self, Self::Position { .. } | Self::Snapshot { .. })
    }

    /// Start and stop events are never dropped.
    fn is_lifecycle(&self) -> bool {
        matches!(self, Self::Started | Self::Stopped)
    }
}

/// Identifies one subscriber for [`EventBus::unsubscribe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubscriberId(u64);

/// Errors from building or subscribing to a bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusError {
    /// A queue must hold at least one event.
    ZeroCapacity,
    /// The bus already has the maximum number of live subscribers.
    SubscriberLimit { max: usize },
}

impl std::fmt::Display for BusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroCapacity => write!(f, "event queue capacity must be at least 1"),
            Self::SubscriberLimit { max } => write!(f, "subscriber limit of {max} reached"),
        }
    }
}

impl std::error::Error for BusError {}

#[derive(Debug)]
struct Queue {
    items: VecDeque<PlaybackBusEvent>,
    capacity: usize,
    dropped: u64,
    quarantined: bool,
    quarantine_dropped: u64,
}

fn lock(queue: &Mutex<Queue>) -> MutexGuard<'_, Queue> {
    // A panic elsewhere cannot leave the queue in a state that is unsafe to read, so the
    // poisoned guard is used.
    match queue.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// The receiving end of one subscriber's bounded queue.
#[derive(Debug)]
pub struct Subscription {
    id: SubscriberId,
    queue: Arc<Mutex<Queue>>,
}

impl Subscription {
    pub fn id(&self) -> SubscriberId {
        self.id
    }

    /// The oldest queued event, if any. Never blocks.
    pub fn try_recv(&self) -> Option<PlaybackBusEvent> {
        lock(&self.queue).items.pop_front()
    }

    /// Events lost for this subscriber because its queue was full.
    pub fn dropped(&self) -> u64 {
        lock(&self.queue).dropped
    }

    /// Events published while this subscriber was quarantined. Counted apart from
    /// [`Subscription::dropped`], so overflow and quarantine stay distinguishable.
    pub fn quarantine_dropped(&self) -> u64 {
        lock(&self.queue).quarantine_dropped
    }

    /// Events currently queued.
    pub fn len(&self) -> usize {
        lock(&self.queue).items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// What one [`EventBus::publish`] call did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PublishReport {
    /// Subscribers that received the event.
    pub delivered: usize,
    /// Events lost for subscribers during this publish (evicted or refused).
    pub dropped: usize,
    /// The event was a progress event that arrived inside the throttle window. Nothing was
    /// delivered and no subscriber lost anything.
    pub throttled: bool,
}

struct Slot {
    id: SubscriberId,
    queue: Arc<Mutex<Queue>>,
}

/// Last known playback state. Replayed to a subscriber that joins mid-playback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Current {
    position_ms: u64,
    duration_ms: Option<u64>,
    paused: bool,
}

/// A bounded fan-out bus for [`PlaybackBusEvent`].
pub struct EventBus {
    queue_capacity: usize,
    max_subscribers: usize,
    next_id: u64,
    subscribers: Vec<Slot>,
    dropped_total: u64,
    throttled_total: u64,
    origin: Instant,
    last_progress_at: Option<Duration>,
    current: Option<Current>,
}

impl EventBus {
    /// A bus whose subscriber queues hold `queue_capacity` events each, with at most
    /// `max_subscribers` live subscribers.
    pub fn new(queue_capacity: usize, max_subscribers: usize) -> Result<Self, BusError> {
        if queue_capacity == 0 {
            return Err(BusError::ZeroCapacity);
        }
        Ok(Self {
            queue_capacity,
            max_subscribers,
            next_id: 1,
            subscribers: Vec::new(),
            dropped_total: 0,
            throttled_total: 0,
            origin: Instant::now(),
            last_progress_at: None,
            current: None,
        })
    }

    /// Add a subscriber. A subscriber whose [`Subscription`] was dropped no longer counts.
    ///
    /// If playback is in progress, the new queue starts with one snapshot of the current
    /// state, so a late subscriber does not have to wait for the next progress event.
    pub fn subscribe(&mut self) -> Result<Subscription, BusError> {
        self.prune();
        if self.subscribers.len() >= self.max_subscribers {
            return Err(BusError::SubscriberLimit {
                max: self.max_subscribers,
            });
        }
        let id = SubscriberId(self.next_id);
        self.next_id += 1;
        let mut items = VecDeque::with_capacity(self.queue_capacity);
        if let Some(current) = self.current {
            items.push_back(PlaybackBusEvent::Snapshot {
                position_ms: current.position_ms,
                duration_ms: current.duration_ms,
                paused: current.paused,
            });
        }
        let queue = Arc::new(Mutex::new(Queue {
            items,
            capacity: self.queue_capacity,
            dropped: 0,
            quarantined: false,
            quarantine_dropped: 0,
        }));
        self.subscribers.push(Slot {
            id,
            queue: Arc::clone(&queue),
        });
        Ok(Subscription { id, queue })
    }

    /// Remove a subscriber. Returns `false` if the id was not subscribed.
    pub fn unsubscribe(&mut self, id: SubscriberId) -> bool {
        let before = self.subscribers.len();
        self.subscribers.retain(|slot| slot.id != id);
        self.subscribers.len() != before
    }

    /// Stop or resume queueing for one subscriber. While quarantined, published events are
    /// counted in [`Subscription::quarantine_dropped`] and not queued. Returns `false` if
    /// the id is not subscribed.
    pub fn set_quarantined(&mut self, id: SubscriberId, quarantined: bool) -> bool {
        match self.subscribers.iter().find(|slot| slot.id == id) {
            Some(slot) => {
                lock(&slot.queue).quarantined = quarantined;
                true
            }
            None => false,
        }
    }

    /// Number of live subscribers.
    pub fn subscriber_count(&self) -> usize {
        self.subscribers
            .iter()
            .filter(|slot| Arc::strong_count(&slot.queue) > 1)
            .count()
    }

    /// Total events lost across all subscribers since the bus was created.
    pub fn dropped_total(&self) -> u64 {
        self.dropped_total
    }

    /// Progress events refused by the throttle since the bus was created.
    pub fn throttled_total(&self) -> u64 {
        self.throttled_total
    }

    /// Deliver `event` at the current time. See [`EventBus::publish_at`].
    pub fn publish(&mut self, event: PlaybackBusEvent) -> PublishReport {
        let now = self.origin.elapsed();
        self.publish_at(now, event)
    }

    /// Deliver `event`, using `now` (time since any fixed origin the caller chooses) for the
    /// progress throttle. Times must not go backwards between calls.
    pub fn publish_at(&mut self, now: Duration, event: PlaybackBusEvent) -> PublishReport {
        self.prune();
        let event = redact_event(event);
        // The state follows every publish, including progress the throttle refuses below.
        self.track(&event);
        if event.is_progress() {
            if let Some(last) = self.last_progress_at
                && now.saturating_sub(last) < PROGRESS_THROTTLE
            {
                self.throttled_total += 1;
                return PublishReport {
                    throttled: true,
                    ..PublishReport::default()
                };
            }
            self.last_progress_at = Some(now);
        }

        let mut report = PublishReport::default();
        for slot in &self.subscribers {
            let mut queue = lock(&slot.queue);
            if queue.quarantined {
                queue.quarantine_dropped += 1;
                continue;
            }
            if queue.items.len() < queue.capacity {
                queue.items.push_back(event.clone());
                report.delivered += 1;
                continue;
            }
            if event.is_lifecycle() {
                // Never dropped: evict progress first, then other events. If the queue
                // holds only start and stop events, the new one is kept past capacity.
                let victim = queue
                    .items
                    .iter()
                    .position(PlaybackBusEvent::is_progress)
                    .or_else(|| queue.items.iter().position(|queued| !queued.is_lifecycle()));
                if let Some(index) = victim {
                    queue.items.remove(index);
                    queue.dropped += 1;
                    self.dropped_total += 1;
                    report.dropped += 1;
                }
                queue.items.push_back(event.clone());
                report.delivered += 1;
                continue;
            }
            // Full: make room by evicting the oldest progress event, if there is one.
            let evicted = queue.items.iter().position(PlaybackBusEvent::is_progress);
            queue.dropped += 1;
            self.dropped_total += 1;
            report.dropped += 1;
            if let Some(index) = evicted {
                queue.items.remove(index);
                queue.items.push_back(event.clone());
                report.delivered += 1;
            }
        }
        report
    }

    /// Remove subscribers whose [`Subscription`] has been dropped.
    fn prune(&mut self) {
        self.subscribers
            .retain(|slot| Arc::strong_count(&slot.queue) > 1);
    }

    /// Update the replayed state from one publish. Stop clears it.
    fn track(&mut self, event: &PlaybackBusEvent) {
        match *event {
            PlaybackBusEvent::Started => {
                self.current = Some(Current {
                    position_ms: 0,
                    duration_ms: None,
                    paused: false,
                });
            }
            PlaybackBusEvent::Position { position_ms } => {
                let duration_ms = self.current.and_then(|c| c.duration_ms);
                let paused = self.current.is_some_and(|c| c.paused);
                self.current = Some(Current {
                    position_ms,
                    duration_ms,
                    paused,
                });
            }
            PlaybackBusEvent::Snapshot {
                position_ms,
                duration_ms,
                paused,
            } => {
                self.current = Some(Current {
                    position_ms,
                    duration_ms,
                    paused,
                });
            }
            PlaybackBusEvent::Paused => {
                if let Some(current) = self.current.as_mut() {
                    current.paused = true;
                }
            }
            PlaybackBusEvent::Resumed => {
                if let Some(current) = self.current.as_mut() {
                    current.paused = false;
                }
            }
            PlaybackBusEvent::Stopped => self.current = None,
            PlaybackBusEvent::TrackChanged { .. } | PlaybackBusEvent::Error { .. } => {}
        }
    }
}

/// Error text can carry a stream URL or a credential, so it is redacted before it is queued.
fn redact_event(event: PlaybackBusEvent) -> PlaybackBusEvent {
    match event {
        PlaybackBusEvent::Error { message } => PlaybackBusEvent::Error {
            message: crate::crash_report::redact(&message),
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_bus(capacity: usize) -> EventBus {
        EventBus::new(capacity, 8).unwrap()
    }

    fn at(secs: u64) -> Duration {
        Duration::from_secs(secs)
    }

    #[test]
    fn fan_out_reaches_both_subscribers_in_order() {
        let mut bus = make_bus(8);
        let a = bus.subscribe().unwrap();
        let b = bus.subscribe().unwrap();

        let report = bus.publish_at(at(0), PlaybackBusEvent::Started);
        bus.publish_at(at(5), PlaybackBusEvent::Position { position_ms: 5_000 });
        bus.publish_at(at(6), PlaybackBusEvent::Paused);

        assert_eq!(report.delivered, 2);
        assert_eq!(report.dropped, 0);
        for sub in [&a, &b] {
            assert_eq!(sub.try_recv(), Some(PlaybackBusEvent::Started));
            assert_eq!(
                sub.try_recv(),
                Some(PlaybackBusEvent::Position { position_ms: 5_000 })
            );
            assert_eq!(sub.try_recv(), Some(PlaybackBusEvent::Paused));
            assert_eq!(sub.try_recv(), None);
        }
        assert_eq!(bus.dropped_total(), 0);
    }

    #[test]
    fn full_subscriber_does_not_block_publish_or_other_subscribers() {
        let mut bus = make_bus(2);
        let slow = bus.subscribe().unwrap();
        let fast = bus.subscribe().unwrap();

        // The slow subscriber never reads. The fast one drains after every publish.
        let mut fast_seen = 0;
        for i in 0..10 {
            bus.publish_at(at(i * 5), PlaybackBusEvent::Position { position_ms: i });
            while fast.try_recv().is_some() {
                fast_seen += 1;
            }
        }

        assert_eq!(fast_seen, 10, "the fast subscriber gets every event");
        assert_eq!(slow.len(), 2, "the slow queue stays at capacity");
        assert_eq!(slow.dropped(), 8);
    }

    #[test]
    fn drop_counter_increments_per_subscriber_and_in_total() {
        let mut bus = make_bus(1);
        let sub = bus.subscribe().unwrap();

        bus.publish(PlaybackBusEvent::Paused);
        let report = bus.publish(PlaybackBusEvent::Resumed);

        assert_eq!(report.dropped, 1);
        assert_eq!(sub.dropped(), 1);
        assert_eq!(bus.dropped_total(), 1);
    }

    #[test]
    fn overflow_evicts_oldest_progress_and_keeps_lifecycle_events() {
        let mut bus = make_bus(3);
        let sub = bus.subscribe().unwrap();

        bus.publish_at(at(0), PlaybackBusEvent::Started);
        for (i, ms) in (1..=4).enumerate() {
            bus.publish_at(
                at(5 * (i as u64 + 1)),
                PlaybackBusEvent::Position { position_ms: ms },
            );
        }
        bus.publish_at(at(30), PlaybackBusEvent::Stopped);

        assert_eq!(sub.try_recv(), Some(PlaybackBusEvent::Started));
        assert_eq!(
            sub.try_recv(),
            Some(PlaybackBusEvent::Position { position_ms: 4 })
        );
        assert_eq!(sub.try_recv(), Some(PlaybackBusEvent::Stopped));
        assert_eq!(sub.try_recv(), None);
        assert_eq!(sub.dropped(), 3);
    }

    #[test]
    fn progress_inside_the_throttle_window_is_refused_and_not_counted_as_loss() {
        let mut bus = make_bus(4);
        let sub = bus.subscribe().unwrap();

        let first = bus.publish_at(at(10), PlaybackBusEvent::Position { position_ms: 1 });
        let inside = bus.publish_at(at(12), PlaybackBusEvent::Position { position_ms: 2 });
        let just_inside = bus.publish_at(
            at(15) - Duration::from_millis(1),
            PlaybackBusEvent::Position { position_ms: 3 },
        );
        let on_boundary = bus.publish_at(at(15), PlaybackBusEvent::Position { position_ms: 4 });

        assert!(!first.throttled);
        assert!(inside.throttled);
        assert_eq!(inside.delivered, 0);
        assert!(just_inside.throttled);
        assert!(
            !on_boundary.throttled,
            "exactly five seconds later is accepted"
        );
        assert_eq!(bus.throttled_total(), 2);
        assert_eq!(sub.len(), 2);
        assert_eq!(sub.dropped(), 0);
    }

    #[test]
    fn snapshot_shares_the_progress_throttle_and_carries_no_locator() {
        let mut bus = make_bus(4);
        let sub = bus.subscribe().unwrap();

        bus.publish_at(at(0), PlaybackBusEvent::Position { position_ms: 100 });
        let refused = bus.publish_at(
            at(1),
            PlaybackBusEvent::Snapshot {
                position_ms: 1_100,
                duration_ms: Some(3_600_000),
                paused: false,
            },
        );
        assert!(refused.throttled);

        let accepted = bus.publish_at(
            at(5),
            PlaybackBusEvent::Snapshot {
                position_ms: 5_100,
                duration_ms: None,
                paused: true,
            },
        );
        assert!(!accepted.throttled);
        assert_eq!(accepted.delivered, 1);
        assert_eq!(
            sub.try_recv(),
            Some(PlaybackBusEvent::Position { position_ms: 100 })
        );
        assert_eq!(
            sub.try_recv(),
            Some(PlaybackBusEvent::Snapshot {
                position_ms: 5_100,
                duration_ms: None,
                paused: true,
            })
        );
    }

    #[test]
    fn snapshot_is_evicted_like_other_progress() {
        let mut bus = make_bus(2);
        let sub = bus.subscribe().unwrap();

        bus.publish_at(
            at(0),
            PlaybackBusEvent::Snapshot {
                position_ms: 0,
                duration_ms: Some(1_000),
                paused: false,
            },
        );
        bus.publish_at(at(1), PlaybackBusEvent::Paused);
        let report = bus.publish_at(at(2), PlaybackBusEvent::Resumed);

        assert_eq!(
            report.dropped, 1,
            "the snapshot is the oldest progress event"
        );
        assert_eq!(report.delivered, 1);
        assert_eq!(sub.dropped(), 1);
        assert_eq!(sub.try_recv(), Some(PlaybackBusEvent::Paused));
        assert_eq!(sub.try_recv(), Some(PlaybackBusEvent::Resumed));
        assert_eq!(sub.try_recv(), None);
    }

    #[test]
    fn start_and_stop_evict_progress_before_other_events() {
        let mut bus = make_bus(2);
        let sub = bus.subscribe().unwrap();

        bus.publish_at(at(0), PlaybackBusEvent::Position { position_ms: 1 });
        bus.publish_at(at(1), PlaybackBusEvent::Paused);
        let report = bus.publish_at(at(2), PlaybackBusEvent::Stopped);

        assert_eq!(report.dropped, 1);
        assert_eq!(report.delivered, 1);
        assert_eq!(sub.try_recv(), Some(PlaybackBusEvent::Paused));
        assert_eq!(sub.try_recv(), Some(PlaybackBusEvent::Stopped));
        assert_eq!(sub.try_recv(), None);
    }

    #[test]
    fn stop_evicts_a_non_lifecycle_event_when_no_progress_is_queued() {
        let mut bus = make_bus(2);
        let sub = bus.subscribe().unwrap();

        bus.publish_at(at(0), PlaybackBusEvent::Paused);
        bus.publish_at(at(1), PlaybackBusEvent::TrackChanged { index: 2 });
        let report = bus.publish_at(at(2), PlaybackBusEvent::Stopped);

        assert_eq!(report.dropped, 1);
        assert_eq!(report.delivered, 1);
        assert_eq!(sub.dropped(), 1);
        assert_eq!(bus.dropped_total(), 1);
        assert_eq!(
            sub.try_recv(),
            Some(PlaybackBusEvent::TrackChanged { index: 2 })
        );
        assert_eq!(sub.try_recv(), Some(PlaybackBusEvent::Stopped));
    }

    #[test]
    fn start_and_stop_are_kept_past_capacity_when_the_queue_holds_only_lifecycle_events() {
        let mut bus = make_bus(2);
        let sub = bus.subscribe().unwrap();

        bus.publish_at(at(0), PlaybackBusEvent::Started);
        bus.publish_at(at(1), PlaybackBusEvent::Stopped);
        let report = bus.publish_at(at(2), PlaybackBusEvent::Started);

        assert_eq!(report.dropped, 0);
        assert_eq!(report.delivered, 1);
        assert_eq!(
            sub.len(),
            3,
            "the lifecycle event waits past the nominal capacity"
        );
        assert_eq!(sub.dropped(), 0);
        assert_eq!(sub.try_recv(), Some(PlaybackBusEvent::Started));
        assert_eq!(sub.try_recv(), Some(PlaybackBusEvent::Stopped));
        assert_eq!(sub.try_recv(), Some(PlaybackBusEvent::Started));
    }

    #[test]
    fn lifecycle_events_are_not_throttled() {
        let mut bus = make_bus(4);
        let sub = bus.subscribe().unwrap();

        bus.publish_at(at(0), PlaybackBusEvent::Started);
        let stop = bus.publish_at(at(0), PlaybackBusEvent::Stopped);

        assert!(!stop.throttled);
        assert_eq!(sub.len(), 2);
        assert_eq!(bus.throttled_total(), 0);
    }

    #[test]
    fn unsubscribe_removes_the_subscriber() {
        let mut bus = make_bus(4);
        let a = bus.subscribe().unwrap();
        let b = bus.subscribe().unwrap();
        assert_eq!(bus.subscriber_count(), 2);

        assert!(bus.unsubscribe(a.id()));
        assert!(!bus.unsubscribe(a.id()), "second unsubscribe finds nothing");
        assert_eq!(bus.subscriber_count(), 1);

        let report = bus.publish(PlaybackBusEvent::Resumed);
        assert_eq!(report.delivered, 1);
        assert_eq!(a.try_recv(), None, "unsubscribed queue receives nothing");
        assert_eq!(b.try_recv(), Some(PlaybackBusEvent::Resumed));
    }

    #[test]
    fn dropped_subscription_is_pruned_and_frees_a_slot() {
        let mut bus = EventBus::new(4, 1).unwrap();
        let first = bus.subscribe().unwrap();
        assert_eq!(
            bus.subscribe().unwrap_err(),
            BusError::SubscriberLimit { max: 1 }
        );

        drop(first);
        assert_eq!(bus.subscriber_count(), 0);
        assert!(bus.subscribe().is_ok());
    }

    #[test]
    fn zero_capacity_is_refused() {
        assert_eq!(EventBus::new(0, 4).err(), Some(BusError::ZeroCapacity));
    }

    #[test]
    fn error_text_with_a_url_or_credential_is_redacted_before_queueing() {
        let mut bus = make_bus(4);
        let sub = bus.subscribe().unwrap();

        bus.publish_at(
            at(0),
            PlaybackBusEvent::Error {
                message: "stream failed: https://10.0.0.5:8096/Videos/1/stream?api_key=SECRET123"
                    .to_owned(),
            },
        );

        match sub.try_recv() {
            Some(PlaybackBusEvent::Error { message }) => {
                assert!(!message.contains("SECRET123"), "{message}");
                assert!(!message.contains("api_key"), "{message}");
                assert!(!message.contains("10.0.0.5"), "{message}");
                assert!(!message.contains("://"), "{message}");
            }
            other => panic!("expected a redacted error event, got {other:?}"),
        }
    }

    #[test]
    fn late_subscriber_starts_with_a_snapshot_of_the_current_state() {
        let mut bus = make_bus(4);
        bus.publish_at(at(0), PlaybackBusEvent::Started);
        bus.publish_at(at(5), PlaybackBusEvent::Position { position_ms: 5_000 });
        bus.publish_at(at(6), PlaybackBusEvent::Paused);

        let late = bus.subscribe().unwrap();
        assert_eq!(
            late.try_recv(),
            Some(PlaybackBusEvent::Snapshot {
                position_ms: 5_000,
                duration_ms: None,
                paused: true,
            })
        );
        assert_eq!(late.try_recv(), None);
    }

    #[test]
    fn late_subscriber_snapshot_follows_throttled_progress() {
        let mut bus = make_bus(4);
        bus.publish_at(at(0), PlaybackBusEvent::Position { position_ms: 1_000 });
        let throttled = bus.publish_at(at(2), PlaybackBusEvent::Position { position_ms: 3_000 });
        assert!(throttled.throttled);

        let late = bus.subscribe().unwrap();
        assert_eq!(
            late.try_recv(),
            Some(PlaybackBusEvent::Snapshot {
                position_ms: 3_000,
                duration_ms: None,
                paused: false,
            }),
            "the refused progress still moves the replayed state"
        );
    }

    #[test]
    fn no_snapshot_for_a_subscriber_that_joins_after_stop() {
        let mut bus = make_bus(4);
        bus.publish_at(at(0), PlaybackBusEvent::Started);
        bus.publish_at(at(1), PlaybackBusEvent::Stopped);

        let late = bus.subscribe().unwrap();
        assert_eq!(late.try_recv(), None);
    }

    #[test]
    fn quarantined_subscriber_gets_nothing_new_and_counts_apart_from_overflow() {
        let mut bus = make_bus(2);
        let plugin = bus.subscribe().unwrap();
        let other = bus.subscribe().unwrap();

        assert!(bus.set_quarantined(plugin.id(), true));
        let report = bus.publish_at(at(0), PlaybackBusEvent::Started);
        bus.publish_at(at(1), PlaybackBusEvent::Paused);

        assert_eq!(
            report.delivered, 1,
            "only the healthy subscriber receives it"
        );
        assert_eq!(plugin.len(), 0);
        assert_eq!(plugin.quarantine_dropped(), 2);
        assert_eq!(plugin.dropped(), 0, "quarantine is not counted as overflow");
        assert_eq!(bus.dropped_total(), 0);
        assert_eq!(other.len(), 2);

        assert!(bus.set_quarantined(plugin.id(), false));
        bus.publish_at(at(2), PlaybackBusEvent::Resumed);
        assert_eq!(plugin.try_recv(), Some(PlaybackBusEvent::Resumed));
        assert_eq!(plugin.quarantine_dropped(), 2);
    }

    #[test]
    fn quarantine_for_an_unknown_subscriber_is_refused() {
        let mut bus = make_bus(2);
        assert!(!bus.set_quarantined(SubscriberId(999), true));
    }
}

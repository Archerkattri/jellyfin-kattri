//! Session epochs: the single staleness boundary for asynchronous work.
//!
//! A media client reconciles three asynchronous authorities: user intent (immediate), server truth
//! (delayed, and *mutable under you*: a reconnect can yield a new session record) and the playback
//! engine (its own thread and clock). The invariant this module makes structural is:
//!
//! > No observation tagged with an older epoch may overwrite state belonging to a newer epoch.
//!
//! Every asynchronous effect is issued under the current [`Epoch`]; every result carries the epoch
//! it was issued under. Callers hand each result to [`accept`] with the current epoch. A result from
//! a previous epoch is dropped there, once, instead of being re-checked by hand in every handler.
//!
//! A reconnect (or sign-in, or sign-out) bumps the epoch with [`Epoch::next`]. Everything issued
//! under the old epoch becomes stale in one step, and any work the new epoch requires (for example
//! re-advertising remote-control capabilities for a new session record) is a declared consequence of
//! that bump rather than a separate wire-up someone can forget.
//!
//! This type carries no I/O and no clock: the caller decides when a bump happens.

use std::fmt;

/// Identifies one generation of session-bound work.
///
/// The value is opaque. Compare epochs with [`Epoch::is_current_for`] or `==`; do not do arithmetic
/// on it, because only the owner of the session knows when a new generation starts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Epoch(u64);

impl Epoch {
    /// The epoch before any session exists. Work issued here is never accepted once a session
    /// starts, which is the correct outcome: there is nothing for it to apply to.
    pub const NONE: Self = Self(0);

    /// The first epoch a session uses.
    pub const FIRST: Self = Self(1);

    /// The successor epoch. Call this exactly when the session identity changes: sign-in,
    /// reconnect to a new session record, or sign-out.
    ///
    /// The counter saturates instead of wrapping. A saturated epoch compares equal to itself, so
    /// the worst case is that the boundary stops distinguishing generations after `u64::MAX` bumps,
    /// never that a stale generation is silently accepted as current.
    pub fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }

    /// Whether an observation issued under `self` may be applied while `current` is the live epoch.
    ///
    /// Only an exact match is current. An observation from an *older* epoch is stale; one from a
    /// *newer* epoch cannot exist for a caller holding `current`, and accepting it would let a
    /// mis-tagged result jump the queue, so it is refused too.
    pub fn is_current_for(self, current: Self) -> bool {
        self == current
    }

    /// The raw counter, for logging and tests. Not for arithmetic.
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for Epoch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "epoch-{}", self.0)
    }
}

/// A value issued under one epoch.
///
/// Tagging is what makes staleness checkable: a plain `Result` loses the information needed to tell
/// a fresh answer from an abandoned one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Tagged<T> {
    epoch: Epoch,
    value: T,
}

impl<T> Tagged<T> {
    /// Attach `epoch` to `value`.
    pub const fn new(epoch: Epoch, value: T) -> Self {
        Self { epoch, value }
    }

    /// The epoch this value was issued under.
    pub const fn epoch(&self) -> Epoch {
        self.epoch
    }

    /// Borrow the value without consuming the tag.
    pub const fn value(&self) -> &T {
        &self.value
    }

    /// Re-tag the value, for example when one epoch's work is deliberately carried into the next.
    /// The caller must be sure that is intended; the boundary does not infer it.
    pub fn retag(self, epoch: Epoch) -> Self {
        Self {
            epoch,
            value: self.value,
        }
    }

    /// Consume the tag and return the value, ignoring its epoch.
    ///
    /// Use only where staleness has already been decided (for example after [`accept`]). Reaching
    /// for this to skip the boundary reintroduces the bug the boundary exists to prevent.
    pub fn into_value(self) -> T {
        self.value
    }
}

/// The single staleness boundary: return the value only when it was issued under `current`.
///
/// Every asynchronous result passes through here. `None` means the result belongs to an abandoned
/// generation and must be dropped without touching state.
pub fn accept<T>(current: Epoch, tagged: Tagged<T>) -> Option<T> {
    tagged.epoch.is_current_for(current).then_some(tagged.value)
}

/// What happened to one result at the boundary, for logging and counters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoundaryOutcome {
    /// The result was current and may be applied.
    Accepted,
    /// The result belonged to another epoch and was dropped.
    Stale {
        /// The epoch the result was issued under.
        issued: Epoch,
        /// The live epoch when it arrived.
        current: Epoch,
    },
}

/// Classify one result without consuming it. The caller still decides to apply the value.
pub fn classify<T>(current: Epoch, tagged: &Tagged<T>) -> BoundaryOutcome {
    if tagged.epoch.is_current_for(current) {
        BoundaryOutcome::Accepted
    } else {
        BoundaryOutcome::Stale {
            issued: tagged.epoch,
            current,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BoundaryOutcome, Epoch, Tagged, accept, classify};

    #[test]
    fn the_first_epoch_is_after_none_and_bumps_monotonically() {
        assert_eq!(Epoch::NONE.get(), 0);
        assert_eq!(Epoch::FIRST.get(), 1);
        assert!(Epoch::FIRST > Epoch::NONE);

        let mut epoch = Epoch::FIRST;
        for expected in 2..=5 {
            epoch = epoch.next();
            assert_eq!(epoch.get(), expected);
            assert!(epoch > Epoch::FIRST);
        }
    }

    #[test]
    fn a_bump_invalidates_work_issued_before_it() {
        let issued = Epoch::FIRST;
        let current = issued.next();

        assert!(!issued.is_current_for(current), "old epoch is stale");
        assert!(current.is_current_for(current), "live epoch is current");
        assert!(
            !current.is_current_for(issued),
            "a newer epoch is not current for an older reader"
        );
    }

    #[test]
    fn accept_returns_the_value_only_for_the_live_epoch() {
        let current = Epoch::FIRST;

        assert_eq!(accept(current, Tagged::new(current, 7_u32)), Some(7));
        assert_eq!(
            accept(current, Tagged::new(current.next(), 7_u32)),
            None,
            "a newer tag cannot jump the queue"
        );
        assert_eq!(
            accept(current, Tagged::new(Epoch::NONE, 7_u32)),
            None,
            "work from before any session never applies"
        );
    }

    #[test]
    fn accept_drops_work_from_every_previous_epoch() {
        let mut current = Epoch::FIRST;
        let stale: Vec<Epoch> = (0..5).map(|_| current).collect();
        for _ in 0..5 {
            current = current.next();
        }
        for issued in stale {
            assert_eq!(accept(current, Tagged::new(issued, "x")), None);
        }
        assert_eq!(accept(current, Tagged::new(current, "x")), Some("x"));
    }

    #[test]
    fn classify_reports_which_epochs_diverged() {
        let current = Epoch::FIRST;
        assert_eq!(
            classify(current, &Tagged::new(current, ())),
            BoundaryOutcome::Accepted
        );
        assert_eq!(
            classify(current, &Tagged::new(Epoch::NONE, ())),
            BoundaryOutcome::Stale {
                issued: Epoch::NONE,
                current
            }
        );
    }

    #[test]
    fn retagging_is_explicit_and_keeps_the_value() {
        let carried = Tagged::new(Epoch::FIRST, 42_u32).retag(Epoch::FIRST.next());
        assert_eq!(carried.epoch(), Epoch::FIRST.next());
        assert_eq!(*carried.value(), 42);
        assert_eq!(carried.into_value(), 42);
    }

    #[test]
    fn a_saturated_epoch_stays_current_with_itself_instead_of_wrapping() {
        let mut epoch = Epoch::FIRST;
        for _ in 0..70 {
            epoch = epoch.next();
        }
        assert!(epoch.get() > 1);
        // Saturation: repeated bumps never wrap back to a value an old tag could match.
        let mut last = epoch;
        for _ in 0..3 {
            let next = last.next();
            assert!(next >= last);
            last = next;
        }
        assert!(last.is_current_for(last));
    }

    #[test]
    fn display_names_the_generation_without_leaking_anything_else() {
        assert_eq!(Epoch::NONE.to_string(), "epoch-0");
        assert_eq!(Epoch::FIRST.to_string(), "epoch-1");
    }
}

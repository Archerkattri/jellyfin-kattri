use std::fmt;

use crate::pin::{HashParams, Pin, PinError, SALT_LEN, StoredPin, generate_salt};

/// Delay added by each consecutive wrong attempt, in seconds.
pub const BACKOFF_STEP_SECS: u64 = 1;
/// Longest delay between attempts, in seconds.
pub const MAX_BACKOFF_SECS: u64 = 30;

/// The delay that follows `failures` consecutive wrong attempts: one step per failure, capped at
/// [`MAX_BACKOFF_SECS`]. With no failures there is no delay.
pub fn backoff_secs(failures: u32) -> u64 {
    u64::from(failures)
        .saturating_mul(BACKOFF_STEP_SECS)
        .min(MAX_BACKOFF_SECS)
}

/// Result of an unlock attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unlock {
    /// No PIN is set, so the profile is not locked.
    NotProtected,
    /// The PIN matched. The failure counter and any delay are cleared.
    Unlocked,
    /// The PIN did not match. `failures` counts consecutive wrong attempts. The next attempt is
    /// refused for `retry_after_secs` seconds.
    WrongPin {
        failures: u32,
        retry_after_secs: u64,
    },
    /// Attempts are refused until the delay ends. Refused attempts are not counted.
    LockedOut { retry_after_secs: u64 },
}

/// The part of the lock that must survive a restart: the consecutive failure count and the Unix
/// time (whole seconds) when the current delay ends.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AttemptState {
    pub failures: u32,
    pub locked_until: Option<u64>,
}

/// The profile lock: an optional stored PIN plus the attempt state.
///
/// `now` is a caller-supplied clock in whole seconds since the Unix epoch. The epoch matters
/// because the delay end is persisted and read back after a restart.
#[derive(Clone, PartialEq, Eq)]
pub struct PinLock {
    stored: Option<StoredPin>,
    failures: u32,
    locked_until: Option<u64>,
}

impl PinLock {
    /// Creates a lock from a stored PIN with no failures recorded, or none for an unprotected
    /// profile.
    pub fn new(stored: Option<StoredPin>) -> Self {
        Self {
            stored,
            failures: 0,
            locked_until: None,
        }
    }

    /// Rebuilds a lock from a stored PIN and the attempt state read from disk. An unprotected
    /// profile carries no attempt state, so `state` is ignored when `stored` is `None`.
    pub fn restore(stored: Option<StoredPin>, state: AttemptState) -> Self {
        if stored.is_none() {
            return Self::new(None);
        }
        Self {
            stored,
            failures: state.failures,
            locked_until: state.locked_until,
        }
    }

    pub fn is_protected(&self) -> bool {
        self.stored.is_some()
    }

    /// The stored record, for the caller to persist.
    pub fn stored(&self) -> Option<&StoredPin> {
        self.stored.as_ref()
    }

    /// The attempt state, for the caller to persist after each attempt.
    pub fn attempt_state(&self) -> AttemptState {
        AttemptState {
            failures: self.failures,
            locked_until: self.locked_until,
        }
    }

    /// Consecutive failed attempts since the last success or the last [`PinLock::clear`].
    pub fn failures(&self) -> u32 {
        self.failures
    }

    /// Sets or replaces the PIN with an explicit salt. Clears any failure count or delay.
    pub fn set_pin(
        &mut self,
        pin: &Pin,
        params: HashParams,
        salt: [u8; SALT_LEN],
    ) -> Result<(), PinError> {
        self.stored = Some(StoredPin::create(pin, params, salt)?);
        self.failures = 0;
        self.locked_until = None;
        Ok(())
    }

    /// Sets or replaces the PIN with a fresh random salt from the operating system.
    pub fn set_pin_random(&mut self, pin: &Pin, params: HashParams) -> Result<(), PinError> {
        let salt = generate_salt()?;
        self.set_pin(pin, params, salt)
    }

    /// Removes the local PIN and all attempt state. This is the documented reset for a forgotten
    /// PIN. It is a local operation only. The caller also deletes the persisted attempt file.
    pub fn clear(&mut self) {
        *self = Self::new(None);
    }

    /// Checks `input` against the stored PIN at time `now`.
    ///
    /// While a delay is running, attempts are refused and not counted. A wrong PIN, including
    /// input that is not a valid PIN, adds one failure and starts a delay of
    /// [`backoff_secs`] seconds. A correct PIN clears the failure count and any delay.
    pub fn try_unlock(&mut self, input: &str, now: u64) -> Result<Unlock, PinError> {
        let Some(stored) = self.stored.as_ref() else {
            return Ok(Unlock::NotProtected);
        };
        if let Some(until) = self.locked_until {
            if now < until {
                return Ok(Unlock::LockedOut {
                    retry_after_secs: until - now,
                });
            }
            self.locked_until = None;
        }
        let matched = match Pin::parse(input) {
            Ok(pin) => stored.matches(&pin)?,
            Err(_) => false,
        };
        if matched {
            self.failures = 0;
            self.locked_until = None;
            return Ok(Unlock::Unlocked);
        }
        self.failures = self.failures.saturating_add(1);
        let delay = backoff_secs(self.failures);
        self.locked_until = Some(now.saturating_add(delay));
        Ok(Unlock::WrongPin {
            failures: self.failures,
            retry_after_secs: delay,
        })
    }
}

impl fmt::Debug for PinLock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PinLock")
            .field("protected", &self.is_protected())
            .field("failures", &self.failures)
            .field("delayed", &self.locked_until.is_some())
            .finish()
    }
}

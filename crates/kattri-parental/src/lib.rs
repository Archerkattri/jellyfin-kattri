//! Parental controls and profile PIN (M4-20).
//!
//! - [`Pin`] is a 4 to 8 digit PIN. Its `Debug` output is redacted and parse errors never echo
//!   the input.
//! - [`StoredPin`] is an Argon2id hash with a per-PIN random salt. Only its record string
//!   ([`StoredPin::to_record`]) is meant for the settings file. The record never contains the PIN.
//! - [`PinLock`] holds an optional stored PIN and the attempt state. Each wrong attempt adds one
//!   second of delay before the next attempt, up to [`MAX_BACKOFF_SECS`] seconds. A correct PIN
//!   clears the state.
//! - [`AttemptState`] is the failure count and delay end. [`save_attempts`] and [`load_attempts`]
//!   keep it in the versioned file [`ATTEMPT_FILE_NAME`] in the parental store directory, written
//!   atomically, so a restart does not reset the delay.
//! - [`check_rating`] compares a server-supplied numeric rating with a ceiling. It does not
//!   reimplement the server's rating rules.
//!
//! The PIN is a local convenience lock, not a security boundary. Anyone with write access to the
//! settings file or the attempt file can delete or replace them. The clock passed to
//! [`PinLock::try_unlock`] is whole seconds since the Unix epoch, so that the persisted delay end
//! means the same thing after a restart. Forgetting the PIN is handled by [`PinLock::clear`] and
//! [`delete_attempts`]. Both are local operations and are not a server action.

#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::todo,
        clippy::unimplemented
    )
)]

mod lock;
mod pin;
mod rating;
mod store;

pub use lock::{AttemptState, BACKOFF_STEP_SECS, MAX_BACKOFF_SECS, PinLock, Unlock, backoff_secs};
pub use pin::{
    DIGEST_LEN, HashParams, MAX_PIN_LEN, MIN_PIN_LEN, Pin, PinError, SALT_LEN, StoredPin,
    generate_salt,
};
pub use rating::{RATING_LADDER, RatingDecision, check_rating, check_rating_text, level_for};
pub use store::{
    ATTEMPT_FILE_NAME, ATTEMPT_FILE_VERSION, AttemptFileError, decode_attempts, delete_attempts,
    encode_attempts, load_attempts, save_attempts,
};

//! Pure connectivity helpers: a consecutive-failure tracker that decides when
//! the server should be treated as offline, and `Retry-After` parsing.
//!
//! Nothing here reads a clock or performs I/O. Callers pass the current Unix
//! time in whole seconds, so every result is deterministic.

use std::time::Duration;

use crate::ErrorClass;

/// Number of consecutive offline-type failures that mark the server offline.
pub const OFFLINE_FAILURE_THRESHOLD: u32 = 2;

/// Tracks whether the server is offline from a sequence of request outcomes.
///
/// Connection-refused, unreachable-host, and timeout failures count toward the
/// offline state. Any response from the server, including an error status,
/// proves it is reachable and clears the state. A success always clears it.
/// TLS, plaintext-refusal, and other unclassified failures neither count nor
/// clear, because they do not show whether the server answered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OfflineTracker {
    consecutive_failures: u32,
}

impl OfflineTracker {
    /// A tracker that starts online with no recorded failures.
    pub const fn new() -> Self {
        Self {
            consecutive_failures: 0,
        }
    }

    /// Record a successful request. This always clears the offline state.
    pub fn record_success(&mut self) {
        self.consecutive_failures = 0;
    }

    /// Record a failed request by its user-facing class.
    pub fn record_error_class(&mut self, class: ErrorClass) {
        match class {
            ErrorClass::Offline | ErrorClass::Timeout => {
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
            }
            ErrorClass::ServerUnavailable
            | ErrorClass::AuthExpired
            | ErrorClass::NotFound
            | ErrorClass::Forbidden => {
                self.consecutive_failures = 0;
            }
            ErrorClass::RefusedPlaintext | ErrorClass::TlsFailure | ErrorClass::Unexpected => {}
        }
    }

    /// Record a request result, classifying any error.
    pub fn record<T>(&mut self, result: &Result<T, crate::ApiError>) {
        match result {
            Ok(_) => self.record_success(),
            Err(error) => self.record_error_class(error.classify()),
        }
    }

    /// Whether the server is currently considered offline.
    pub const fn is_offline(&self) -> bool {
        self.consecutive_failures >= OFFLINE_FAILURE_THRESHOLD
    }

    /// The length of the current run of offline-type failures.
    pub const fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }
}

/// Parse a `Retry-After` header value into a wait relative to `now_unix_secs`.
///
/// Two forms are accepted: a non-negative count of seconds, and an IMF-fixdate
/// HTTP-date such as `Sun, 06 Nov 1994 08:49:37 GMT`. A date in the past yields
/// a zero wait. Anything else, including the obsolete RFC 850 and asctime date
/// forms, returns `None`.
pub fn parse_retry_after(value: &str, now_unix_secs: i64) -> Option<Duration> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if value.bytes().all(|byte| byte.is_ascii_digit()) {
        return value.parse::<u64>().ok().map(Duration::from_secs);
    }
    let target = parse_imf_fixdate(value)?;
    let wait = target.saturating_sub(now_unix_secs).max(0);
    Some(Duration::from_secs(u64::try_from(wait).ok()?))
}

/// Parse an IMF-fixdate into Unix seconds.
fn parse_imf_fixdate(value: &str) -> Option<i64> {
    let parts: Vec<&str> = value.split(' ').collect();
    let [weekday, day, month, year, time, zone] = parts.as_slice() else {
        return None;
    };
    if !weekday.ends_with(',') || *zone != "GMT" {
        return None;
    }
    let day = parse_fixed_digits(day, 2)?;
    let month = month_number(month)?;
    let year = parse_fixed_digits(year, 4)?;
    let mut clock = time.split(':');
    let hour = parse_fixed_digits(clock.next()?, 2)?;
    let minute = parse_fixed_digits(clock.next()?, 2)?;
    let second = parse_fixed_digits(clock.next()?, 2)?;
    if clock.next().is_some() {
        return None;
    }
    if !(1..=days_in_month(year, month)).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

fn parse_fixed_digits(value: &str, width: usize) -> Option<i64> {
    if value.len() != width || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn month_number(name: &str) -> Option<i64> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    MONTHS
        .iter()
        .position(|month| *month == name)
        .and_then(|index| i64::try_from(index + 1).ok())
}

const fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if is_leap_year(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

const fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Days since 1970-01-01 in the proleptic Gregorian calendar.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted_month = (month + 9) % 12;
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{OFFLINE_FAILURE_THRESHOLD, OfflineTracker, parse_retry_after};
    use crate::{ApiError, ErrorClass};

    /// `Sun, 06 Nov 1994 08:49:37 GMT`, the example date in RFC 9110.
    const RFC_DATE_UNIX: i64 = 784_111_777;

    #[test]
    fn one_failure_is_not_offline_and_two_consecutive_failures_are() {
        let mut tracker = OfflineTracker::new();
        tracker.record_error_class(ErrorClass::Offline);
        assert!(!tracker.is_offline());
        tracker.record_error_class(ErrorClass::Offline);
        assert!(tracker.is_offline());
        assert_eq!(tracker.consecutive_failures(), OFFLINE_FAILURE_THRESHOLD);
    }

    #[test]
    fn timeouts_count_toward_the_offline_streak() {
        let mut tracker = OfflineTracker::new();
        tracker.record_error_class(ErrorClass::Timeout);
        tracker.record_error_class(ErrorClass::Timeout);
        assert!(tracker.is_offline());
    }

    #[test]
    fn a_success_clears_the_offline_state() {
        let mut tracker = OfflineTracker::new();
        tracker.record_error_class(ErrorClass::Offline);
        tracker.record_error_class(ErrorClass::Offline);
        assert!(tracker.is_offline());
        tracker.record_success();
        assert!(!tracker.is_offline());
        assert_eq!(tracker.consecutive_failures(), 0);
    }

    #[test]
    fn a_response_breaks_the_streak_and_restarts_the_count() {
        let mut tracker = OfflineTracker::new();
        tracker.record_error_class(ErrorClass::Offline);
        tracker.record_error_class(ErrorClass::Offline);
        tracker.record_error_class(ErrorClass::ServerUnavailable);
        assert!(!tracker.is_offline());
        tracker.record_error_class(ErrorClass::Offline);
        assert!(
            !tracker.is_offline(),
            "the count restarted after a response"
        );
    }

    #[test]
    fn any_http_response_proves_reachability_and_clears_the_state() {
        for class in [
            ErrorClass::ServerUnavailable,
            ErrorClass::AuthExpired,
            ErrorClass::NotFound,
            ErrorClass::Forbidden,
        ] {
            let mut tracker = OfflineTracker::new();
            tracker.record_error_class(ErrorClass::Offline);
            tracker.record_error_class(ErrorClass::Offline);
            tracker.record_error_class(class);
            assert!(!tracker.is_offline(), "{class:?}");
        }
    }

    #[test]
    fn tls_refusal_and_unexpected_failures_neither_count_nor_clear() {
        for class in [
            ErrorClass::TlsFailure,
            ErrorClass::RefusedPlaintext,
            ErrorClass::Unexpected,
        ] {
            let mut tracker = OfflineTracker::new();
            tracker.record_error_class(ErrorClass::Offline);
            tracker.record_error_class(class);
            tracker.record_error_class(ErrorClass::Offline);
            assert!(tracker.is_offline(), "{class:?} must not break the streak");
        }
    }

    #[test]
    fn record_classifies_api_results() {
        let mut tracker = OfflineTracker::new();
        let failed: Result<(), ApiError> = Err(ApiError::Unreachable);
        tracker.record(&failed);
        tracker.record(&failed);
        assert!(tracker.is_offline());
        let ok: Result<(), ApiError> = Ok(());
        tracker.record(&ok);
        assert!(!tracker.is_offline());
    }

    #[test]
    fn retry_after_accepts_delta_seconds() {
        assert_eq!(parse_retry_after("0", 0), Some(Duration::ZERO));
        assert_eq!(parse_retry_after("120", 0), Some(Duration::from_secs(120)));
        assert_eq!(parse_retry_after("  7  ", 0), Some(Duration::from_secs(7)));
    }

    #[test]
    fn retry_after_rejects_malformed_or_overflowing_seconds() {
        for value in [
            "",
            "   ",
            "-1",
            "1.5",
            "+3",
            "abc",
            "3 seconds",
            "99999999999999999999999",
        ] {
            assert_eq!(parse_retry_after(value, 0), None, "{value:?}");
        }
    }

    #[test]
    fn retry_after_accepts_imf_fixdate_relative_to_now() {
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT", RFC_DATE_UNIX - 100),
            Some(Duration::from_secs(100))
        );
    }

    #[test]
    fn retry_after_date_in_the_past_is_a_zero_wait() {
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT", RFC_DATE_UNIX + 3_600),
            Some(Duration::ZERO)
        );
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT", RFC_DATE_UNIX),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn retry_after_date_handles_the_epoch_and_leap_days() {
        assert_eq!(
            parse_retry_after("Thu, 01 Jan 1970 00:00:00 GMT", -5),
            Some(Duration::from_secs(5))
        );
        assert!(parse_retry_after("Thu, 29 Feb 2024 12:00:00 GMT", 0).is_some());
    }

    #[test]
    fn retry_after_rejects_invalid_http_dates() {
        for value in [
            "Sun, 31 Feb 1994 08:49:37 GMT",
            "Sun, 29 Feb 1994 08:49:37 GMT",
            "Sun, 06 Nov 1994 24:00:00 GMT",
            "Sun, 06 Nov 1994 08:60:00 GMT",
            "Sun, 06 Nov 1994 08:49:37 PST",
            "Sun, 06 Nov 1994 08:49 GMT",
            "Sun, 06 Nov 1994 08:49:37:00 GMT",
            "Sun, 06 Foo 1994 08:49:37 GMT",
            "Sun 06 Nov 1994 08:49:37 GMT",
            "Sunday, 06-Nov-94 08:49:37 GMT",
            "Sun, 6 Nov 1994 08:49:37 GMT",
        ] {
            assert_eq!(parse_retry_after(value, 0), None, "{value:?}");
        }
    }
}

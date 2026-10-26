//! Server clock sampler for SyncPlay (M4-11). The task runs on the worker runtime. It reads
//! `GET /GetUtcTime` once after sign-in, and again every [`SAMPLE_INTERVAL`] while the worker
//! has joined a group. Each reading goes to the app as a [`WorkerEvent::ServerClockSampled`],
//! and the app feeds the estimator. The sampler never reads playback state.
//!
//! The task lives while its [`ClockSamplerHandle`] does. The worker drops the handle when the
//! session ends, the same way it drops the session socket.

use std::time::{Duration, Instant};

use jellyfin_api::system_time::UtcTimeResponse;
use jellyfin_api::{JellyfinClient, Session};
use kattri_client_core::syncplay::parse_server_timestamp_ms;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::{WorkerEvent, WorkerSink, send_event};

/// How often the clock is read while a group is joined. It matches the estimator's sample
/// window, so the estimate is refreshed before its samples age out.
pub(super) const SAMPLE_INTERVAL: Duration = Duration::from_secs(30);

/// Whether a clock read is due. The first read after sign-in is always due. Later reads
/// happen only while a group is joined, once per interval.
pub(super) fn sample_due(joined: bool, since_last: Option<Duration>) -> bool {
    match since_last {
        None => true,
        Some(elapsed) => joined && elapsed >= SAMPLE_INTERVAL,
    }
}

/// How long the task waits before it checks again. While joined, it waits out the rest of
/// the interval. Otherwise it waits a full interval, and a join wakes it early.
pub(super) fn wait_before_check(joined: bool, since_last: Option<Duration>) -> Duration {
    match (joined, since_last) {
        (true, Some(elapsed)) => SAMPLE_INTERVAL.saturating_sub(elapsed),
        (true, None) => Duration::ZERO,
        (false, _) => SAMPLE_INTERVAL,
    }
}

/// The server's reply time in Unix milliseconds. The estimator assumes the server stamped
/// its reply about halfway through the round trip, so the transmission time is the one used.
/// `None` when the field is not in the server's UTC form.
fn reply_time_ms(reply: &UtcTimeResponse) -> Option<i64> {
    parse_server_timestamp_ms(&reply.response_transmission_time).ok()
}

/// Owns the sampler task. Dropping the handle aborts it.
pub(super) struct ClockSamplerHandle {
    joined: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl ClockSamplerHandle {
    /// Tell the task whether a group is joined. A change wakes the task at once.
    pub(super) fn set_joined(&self, joined: bool) {
        self.joined.send_replace(joined);
    }
}

impl Drop for ClockSamplerHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Start the sampler for a signed-in session. Returns `None` for a sink that is not live (the
/// worker tests use one, because their fixtures count every request) and when there is no
/// session. Must be called inside the worker runtime.
pub(super) fn start(
    client: Option<&JellyfinClient>,
    session: Option<&Session>,
    events: &WorkerSink,
) -> Option<ClockSamplerHandle> {
    if !events.server_clock {
        return None;
    }
    let client = client?.clone();
    let session = session?.clone();
    let events = events.clone();
    let (joined, mut joined_rx) = watch::channel(false);
    let task = tokio::spawn(async move {
        let mut last_read: Option<Instant> = None;
        loop {
            let since_last = last_read.map(|at| at.elapsed());
            if sample_due(*joined_rx.borrow(), since_last) {
                last_read = Some(Instant::now());
                read_once(&client, &session, &events).await;
            }
            let since_last = last_read.map(|at| at.elapsed());
            let wait = wait_before_check(*joined_rx.borrow(), since_last);
            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                changed = joined_rx.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
            }
        }
    });
    Some(ClockSamplerHandle { joined, task })
}

/// One timed clock read. The request is bracketed by monotonic times, which the app turns
/// into the estimator's local clock. Errors are logged without their details, which can
/// carry the server address.
async fn read_once(client: &JellyfinClient, session: &Session, events: &WorkerSink) {
    let sent_at = Instant::now();
    let result = client.get_utc_time(session).await;
    let received_at = Instant::now();
    let reply = match result {
        Ok(reply) => reply,
        Err(_) => {
            tracing::debug!("server clock read failed");
            return;
        }
    };
    let Some(server_ms) = reply_time_ms(&reply) else {
        tracing::debug!("server clock reply is not in UTC form");
        return;
    };
    send_event(
        events,
        &events.ctx,
        WorkerEvent::ServerClockSampled {
            sent_at,
            received_at,
            server_ms,
        },
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(value: u64) -> Duration {
        Duration::from_secs(value)
    }

    #[test]
    fn the_first_read_after_sign_in_is_due_even_without_a_group() {
        assert!(sample_due(false, None));
        assert!(sample_due(true, None));
    }

    #[test]
    fn no_read_is_due_while_no_group_is_joined() {
        assert!(!sample_due(false, Some(secs(30))));
        assert!(!sample_due(false, Some(secs(600))));
    }

    #[test]
    fn joined_reads_are_due_once_per_interval() {
        assert!(!sample_due(true, Some(secs(29))));
        assert!(sample_due(true, Some(SAMPLE_INTERVAL)));
        assert!(sample_due(true, Some(secs(31))));
    }

    #[test]
    fn wait_is_the_rest_of_the_interval_while_joined() {
        assert_eq!(wait_before_check(true, Some(secs(10))), secs(20));
        assert_eq!(wait_before_check(true, Some(secs(40))), Duration::ZERO);
        assert_eq!(wait_before_check(true, None), Duration::ZERO);
    }

    #[test]
    fn wait_is_a_full_interval_when_not_joined() {
        assert_eq!(wait_before_check(false, Some(secs(5))), SAMPLE_INTERVAL);
        assert_eq!(wait_before_check(false, None), SAMPLE_INTERVAL);
    }

    #[test]
    fn reply_time_is_the_transmission_time_in_unix_milliseconds() {
        let reply = UtcTimeResponse {
            request_reception_time: "1970-01-01T00:00:01.0000000Z".into(),
            response_transmission_time: "1970-01-01T00:00:02.5000000Z".into(),
        };
        assert_eq!(reply_time_ms(&reply), Some(2_500));
    }

    #[test]
    fn a_reply_without_a_utc_transmission_time_gives_no_time() {
        let missing = UtcTimeResponse {
            request_reception_time: "1970-01-01T00:00:01.0000000Z".into(),
            ..UtcTimeResponse::default()
        };
        assert_eq!(reply_time_ms(&missing), None);

        let local = UtcTimeResponse {
            response_transmission_time: "1970-01-01T00:00:02.0000000+02:00".into(),
            ..UtcTimeResponse::default()
        };
        assert_eq!(reply_time_ms(&local), None, "a local offset is refused");
    }
}

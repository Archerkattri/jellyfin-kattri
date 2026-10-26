//! Debug-only, bounded measurements for native frame and playback timing.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const MAX_FRAME_SAMPLES: usize = 2048;

#[derive(Default)]
struct Metrics {
    started: Option<Instant>,
    frames: VecDeque<Duration>,
    first_ui_swap_elapsed: Option<Duration>,
    first_video_elapsed: Option<Duration>,
    queued_at: Option<Instant>,
    queue_to_first_swap: Option<Duration>,
}

impl Metrics {
    fn start(&mut self, at: Instant) {
        *self = Self {
            started: Some(at),
            ..Self::default()
        };
    }

    fn frame(&mut self, started: Instant, finished: Instant) {
        let Some(elapsed) = finished.checked_duration_since(started) else {
            return;
        };
        if self.first_ui_swap_elapsed.is_none() {
            self.first_ui_swap_elapsed = self
                .started
                .and_then(|session_start| finished.checked_duration_since(session_start));
        }
        if self.frames.len() == MAX_FRAME_SAMPLES {
            self.frames.pop_front();
        }
        self.frames.push_back(elapsed);
    }

    fn playback_queued(&mut self, at: Instant) {
        self.queued_at = Some(at);
        self.queue_to_first_swap = None;
    }

    fn video_presented(&mut self, at: Instant) {
        if self.first_video_elapsed.is_none() {
            self.first_video_elapsed = self
                .started
                .and_then(|started| at.checked_duration_since(started));
        }
        if self.queue_to_first_swap.is_none() {
            self.queue_to_first_swap = self
                .queued_at
                .take()
                .and_then(|queued| at.checked_duration_since(queued));
        }
    }
}

static METRICS: Mutex<Metrics> = Mutex::new(Metrics {
    started: None,
    frames: VecDeque::new(),
    first_ui_swap_elapsed: None,
    first_video_elapsed: None,
    queued_at: None,
    queue_to_first_swap: None,
});

/// Starts a fresh measurement session at the supplied monotonic time.
pub fn start(at: Instant) {
    let mut metrics = METRICS.lock().unwrap_or_else(|error| error.into_inner());
    metrics.start(at);
}

/// Records render-entry-through-buffer-swap duration using an injectable clock.
pub fn frame(started: Instant, finished: Instant) {
    let mut metrics = METRICS.lock().unwrap_or_else(|error| error.into_inner());
    metrics.frame(started, finished);
}

/// Marks a playback queue operation as the next one-shot presentation measurement.
pub fn playback_queued(at: Instant) {
    let mut metrics = METRICS.lock().unwrap_or_else(|error| error.into_inner());
    metrics.playback_queued(at);
}

/// Records the first video frame's successful swap for startup and queue timing.
pub fn video_presented(at: Instant) {
    let mut metrics = METRICS.lock().unwrap_or_else(|error| error.into_inner());
    metrics.video_presented(at);
}

fn percentile(sorted: &[Duration], percentile: usize) -> Option<Duration> {
    if sorted.is_empty() {
        return None;
    }
    // Nearest-rank percentile (rank = ceil(p * n)), converted to a zero-based index.
    let index = (percentile * sorted.len()).div_ceil(100).saturating_sub(1);
    sorted.get(index).copied()
}

fn millis(value: Option<Duration>) -> String {
    value
        .map(|duration| format!("{:.3}", duration.as_secs_f64() * 1000.0))
        .unwrap_or_else(|| "null".to_owned())
}

fn format_report(
    samples: &mut [Duration],
    first_ui_swap: Option<Duration>,
    first_video_swap: Option<Duration>,
    queued: Option<Duration>,
) -> String {
    samples.sort_unstable();
    let p50 = percentile(samples, 50);
    let p95 = percentile(samples, 95);
    let max = samples.last().copied();
    format!(
        "measurement_scope=render_entry_through_buffer_swap\n\
         frame_sample_count={}\n\
         frame_p50_ms={}\n\
         frame_p95_ms={}\n\
         frame_max_ms={}\n\
         startup_first_ui_swap_elapsed_ms={}\n\
         first_video_swap_elapsed_ms={}\n\
         playback_queue_to_first_video_swap_ms={}\n\
         disclaimer=Measurements only; queue-to-swap is not network, play-click, decoded-frame, or perceived latency.\n",
        samples.len(),
        millis(p50),
        millis(p95),
        millis(max),
        millis(first_ui_swap),
        millis(first_video_swap),
        millis(queued),
    )
}

/// Writes a sanitized, numeric-only evidence report in `KATTRI_DEBUG_DIR`.
pub fn dump() -> std::io::Result<()> {
    let directory = std::env::var_os("KATTRI_DEBUG_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "KATTRI_DEBUG_DIR is not configured",
            )
        })?;
    let (mut samples, first_ui_swap, first_video_swap, queued) = {
        let metrics = METRICS.lock().unwrap_or_else(|error| error.into_inner());
        (
            metrics.frames.iter().copied().collect::<Vec<_>>(),
            metrics.first_ui_swap_elapsed,
            metrics.first_video_elapsed,
            metrics.queue_to_first_swap,
        )
    };
    let report = format_report(&mut samples, first_ui_swap, first_video_swap, queued);
    std::fs::write(directory.join("qa-metrics.txt"), report)
}

#[cfg(test)]
mod tests {
    use super::{MAX_FRAME_SAMPLES, Metrics, format_report, percentile};
    use std::time::{Duration, Instant};

    #[test]
    fn nearest_rank_percentiles_are_deterministic() {
        let values = [1, 2, 3, 4, 5].map(Duration::from_millis);
        assert_eq!(percentile(&values, 50), Some(Duration::from_millis(3)));
        assert_eq!(percentile(&values, 95), Some(Duration::from_millis(5)));
        assert_eq!(percentile(&[], 50), None);
    }

    #[test]
    fn empty_report_uses_unknown_values_without_fabricating_samples() {
        let report = format_report(&mut [], None, None, None);
        assert!(report.contains("frame_sample_count=0"));
        assert!(report.contains("frame_p50_ms=null"));
        assert!(report.contains("frame_p95_ms=null"));
        assert!(report.contains("frame_max_ms=null"));
        assert!(report.contains("startup_first_ui_swap_elapsed_ms=null"));
        assert!(report.contains("first_video_swap_elapsed_ms=null"));
        assert!(report.contains("playback_queue_to_first_video_swap_ms=null"));
    }

    #[test]
    fn frame_samples_keep_only_the_latest_bounded_window() {
        let mut metrics = Metrics::default();
        let origin = Instant::now();
        for value in 0..=MAX_FRAME_SAMPLES {
            let began = origin + Duration::from_millis(value as u64);
            metrics.frame(began, began + Duration::from_millis(value as u64 + 1));
        }
        assert_eq!(metrics.frames.len(), MAX_FRAME_SAMPLES);
        assert_eq!(metrics.frames.front(), Some(&Duration::from_millis(2)));
    }

    #[test]
    fn presentation_and_queue_measurements_are_oneshot_and_resettable() {
        let mut metrics = Metrics::default();
        let origin = Instant::now();
        metrics.start(origin);
        metrics.playback_queued(origin + Duration::from_millis(20));
        metrics.video_presented(origin + Duration::from_millis(75));
        metrics.video_presented(origin + Duration::from_millis(100));
        assert_eq!(metrics.first_video_elapsed, Some(Duration::from_millis(75)));
        assert_eq!(metrics.queue_to_first_swap, Some(Duration::from_millis(55)));
        metrics.start(origin + Duration::from_secs(1));
        assert!(metrics.frames.is_empty());
        assert_eq!(metrics.first_ui_swap_elapsed, None);
        assert_eq!(metrics.first_video_elapsed, None);
        assert_eq!(metrics.queue_to_first_swap, None);
    }

    #[test]
    fn first_ui_swap_is_distinct_from_first_video_swap() {
        let mut metrics = Metrics::default();
        let origin = Instant::now();
        metrics.start(origin);
        metrics.frame(
            origin + Duration::from_millis(5),
            origin + Duration::from_millis(10),
        );
        metrics.video_presented(origin + Duration::from_secs(8));
        assert_eq!(
            metrics.first_ui_swap_elapsed,
            Some(Duration::from_millis(10))
        );
        assert_eq!(metrics.first_video_elapsed, Some(Duration::from_secs(8)));
    }
}

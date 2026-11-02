//! Bounded batch enqueue for playlist, collection and album downloads.
//!
//! A batch is a list of [`BatchItem`] values taken from an item listing. Enqueueing reuses
//! [`DownloadManager::start`] and its [`crate::driver::OriginalFileRequest`] path, so the
//! caller supplies item IDs and display names only: no URL, media source, or playback
//! selection is accepted or trusted. The download source for every item is the same
//! `/Items/{itemId}/Download` route a single-item download uses.
//!
//! The batch has a hard item cap ([`MAX_BATCH_ITEMS`]). IDs are deduplicated
//! case-insensitively in favor of their first occurrence, and an item that already has an
//! active or completed job is reported as skipped rather than failed. The caller still
//! enforces the server's `EnableContentDownloading` policy before calling this; the manager
//! only transfers.

use std::collections::HashSet;

use jellyfin_api::SessionAuthorization;

use crate::manager::{DownloadManager, ManagerError, is_valid_id};

/// Most items one batch may contain.
pub const MAX_BATCH_ITEMS: usize = 200;
/// Longest display name kept in a report. Longer names are truncated on a character boundary.
pub const MAX_BATCH_NAME_CHARS: usize = 512;
/// Display name used when the caller supplies only whitespace.
const UNTITLED: &str = "Untitled item";

/// One item to enqueue. `name` is display text for the caller and is never persisted.
///
/// The download source is rebuilt from `item_id` by the same original-file request path a
/// single-item download uses, so this type deliberately carries no URL or media source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BatchItem {
    pub item_id: String,
    pub name: String,
}

impl BatchItem {
    /// Builds an item request. IDs are validated later by the manager, which keeps the
    /// same rules for single and batched downloads.
    pub fn new(item_id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            item_id: item_id.into(),
            name: name.into(),
        }
    }
}

/// Why an item was not enqueued.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BatchSkipReason {
    /// Another copy of the same ID appeared earlier in the same batch.
    Duplicate,
    /// The manager already tracks an active or completed job for the item.
    AlreadyTracked,
}

/// An item the batch deliberately did not start.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BatchSkipped {
    pub item: BatchItem,
    pub reason: BatchSkipReason,
}

/// An item the batch could not start.
#[derive(Debug)]
pub struct BatchFailed {
    pub item: BatchItem,
    pub error: ManagerError,
}

/// What happened to every item in one enqueue call.
///
/// Each input occurrence appears in exactly one of the three lists; within a list, items keep
/// their input order. A repeated ID is not started again and is listed under `skipped` with
/// [`BatchSkipReason::Duplicate`]. The report keeps the display names in memory so the UI can
/// label rows without persisting titles.
#[derive(Debug, Default)]
pub struct BatchReport {
    pub started: Vec<BatchItem>,
    pub skipped: Vec<BatchSkipped>,
    pub failed: Vec<BatchFailed>,
}

impl BatchReport {
    /// Jobs the manager accepted and started.
    #[must_use]
    pub fn started_count(&self) -> usize {
        self.started.len()
    }

    /// Items deliberately not started because of a duplicate or an existing job.
    #[must_use]
    pub fn skipped_count(&self) -> usize {
        self.skipped.len()
    }

    /// Items whose start was refused or failed.
    #[must_use]
    pub fn failed_count(&self) -> usize {
        self.failed.len()
    }
}

/// A batch-level refusal that happens before any item is started.
#[derive(Debug, thiserror::Error)]
pub enum BatchError {
    #[error("server ID must be 1 to 64 ASCII letters, digits or '-'")]
    InvalidServerId,
    #[error("a batch can hold at most {max} items, but {count} were given")]
    TooManyItems { count: usize, max: usize },
}

impl DownloadManager {
    /// Starts a downloadable job for each accepted item, capped by [`MAX_BATCH_ITEMS`].
    ///
    /// The batch is not atomic: items are started one by one, and a failure after an earlier
    /// item started leaves that earlier job running. Every outcome is reported in
    /// [`BatchReport`]; the only batch-level errors are an invalid server ID and an oversized
    /// input. An empty slice is accepted and returns an empty report.
    ///
    /// IDs that repeat within the slice are started once, at their first occurrence. An item
    /// the manager already tracks is reported as [`BatchSkipReason::AlreadyTracked`]. A job in
    /// `Failed` state is retried by the single-item path, not treated as a duplicate.
    pub fn start_batch(
        &self,
        server_id: &str,
        base: &str,
        items: &[BatchItem],
        auth: &SessionAuthorization,
    ) -> Result<BatchReport, BatchError> {
        validate_batch(server_id, items.len())?;
        Ok(dispatch(items, |item| {
            self.start(server_id, base, &item.item_id, auth)
        }))
    }
}

fn validate_batch(server_id: &str, item_count: usize) -> Result<(), BatchError> {
    if !is_valid_id(server_id) {
        return Err(BatchError::InvalidServerId);
    }
    if item_count > MAX_BATCH_ITEMS {
        return Err(BatchError::TooManyItems {
            count: item_count,
            max: MAX_BATCH_ITEMS,
        });
    }
    Ok(())
}

/// Deduplicates and starts in one pass so the report keeps the caller's order without
/// starting a duplicate even briefly.
fn dispatch<F>(items: &[BatchItem], mut start: F) -> BatchReport
where
    F: FnMut(&BatchItem) -> Result<(), ManagerError>,
{
    let mut report = BatchReport::default();
    let mut seen = HashSet::with_capacity(items.len());
    for item in items {
        let item = BatchItem {
            item_id: item.item_id.clone(),
            name: bounded_name(&item.name),
        };
        if !seen.insert(item.item_id.to_ascii_lowercase()) {
            report.skipped.push(BatchSkipped {
                item,
                reason: BatchSkipReason::Duplicate,
            });
            continue;
        }
        match start(&item) {
            Ok(()) => report.started.push(item),
            Err(ManagerError::AlreadyTracked) => report.skipped.push(BatchSkipped {
                item,
                reason: BatchSkipReason::AlreadyTracked,
            }),
            Err(error) => report.failed.push(BatchFailed { item, error }),
        }
    }
    report
}

/// Trims and bounds a display name. The name is never persisted, so this is only a bound on
/// what the report and the caller's in-memory title map can hold.
fn bounded_name(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return UNTITLED.to_owned();
    }
    trimmed.chars().take(MAX_BATCH_NAME_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, name: &str) -> BatchItem {
        BatchItem::new(id, name)
    }

    #[test]
    fn hard_cap_accepts_the_limit_and_rejects_one_more() {
        assert!(validate_batch("server-1", MAX_BATCH_ITEMS).is_ok());
        assert!(matches!(
            validate_batch("server-1", MAX_BATCH_ITEMS + 1),
            Err(BatchError::TooManyItems {
                count,
                max: MAX_BATCH_ITEMS
            }) if count == MAX_BATCH_ITEMS + 1
        ));
    }

    #[test]
    fn invalid_server_id_is_refused_before_dispatch() {
        assert!(matches!(
            validate_batch("", 1),
            Err(BatchError::InvalidServerId)
        ));
        assert!(matches!(
            validate_batch("server id", 1),
            Err(BatchError::InvalidServerId)
        ));
        assert!(validate_batch("server-1", 0).is_ok());
    }

    #[test]
    fn repeated_ids_start_once_at_their_first_occurrence() {
        let items = [
            item("ABC", "First"),
            item("abc", "Second"),
            item("def", "Third"),
            item("ABC", "Fourth"),
        ];
        let mut started = Vec::new();
        let report = dispatch(&items, |item| {
            started.push(item.item_id.clone());
            Ok(())
        });

        assert_eq!(started, ["ABC", "def"]);
        assert_eq!(report.started_count(), 2);
        assert_eq!(report.skipped_count(), 2);
        assert!(report.failed.is_empty());
        assert_eq!(report.started[0].name, "First");
        assert_eq!(report.skipped[0].reason, BatchSkipReason::Duplicate);
        assert_eq!(report.skipped[0].item.name, "Second");
        assert_eq!(report.skipped[1].reason, BatchSkipReason::Duplicate);
    }

    #[test]
    fn existing_jobs_are_skipped_and_other_start_errors_are_reported() {
        let items = [item("a", "A"), item("b", "B"), item("c", "C")];
        let report = dispatch(&items, |item| match item.item_id.as_str() {
            "a" => Ok(()),
            "b" => Err(ManagerError::AlreadyTracked),
            _ => Err(ManagerError::ThreadStart),
        });

        assert_eq!(report.started_count(), 1);
        assert_eq!(report.started[0].item_id, "a");
        assert_eq!(report.skipped_count(), 1);
        assert_eq!(report.skipped[0].item.item_id, "b");
        assert_eq!(report.skipped[0].reason, BatchSkipReason::AlreadyTracked);
        assert_eq!(report.failed_count(), 1);
        assert_eq!(report.failed[0].item.item_id, "c");
        assert!(matches!(report.failed[0].error, ManagerError::ThreadStart));
    }

    #[test]
    fn names_are_trimmed_bounded_and_never_empty() {
        let long = "x".repeat(MAX_BATCH_NAME_CHARS + 20);
        let items = [item("a", &long), item("b", "   ")];
        let report = dispatch(&items, |_| Ok(()));

        assert_eq!(report.started[0].name.chars().count(), MAX_BATCH_NAME_CHARS);
        assert_eq!(report.started[1].name, UNTITLED);
    }

    #[test]
    fn an_empty_batch_reports_nothing_and_starts_nothing() {
        let report = dispatch(&[], |_| panic!("no item may start"));
        assert_eq!(report.started_count(), 0);
        assert_eq!(report.skipped_count(), 0);
        assert_eq!(report.failed_count(), 0);
    }
}

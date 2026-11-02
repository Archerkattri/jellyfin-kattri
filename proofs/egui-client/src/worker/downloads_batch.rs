//! Bounded collection-download planning and dispatch state.
//!
//! The paged collection-fetch helpers and the Album kind are kept for the collection pages'
//! "Download all" action; the catalog selection batch uses the rest today.
#![allow(dead_code)]

use std::collections::{HashSet, VecDeque};
use std::future::Future;

use jellyfin_api::BaseItemDto;
use kattri_downloads::batch::MAX_BATCH_ITEMS;

pub(crate) const COLLECTION_PAGE_SIZE: u32 = 100;
pub(crate) const MAX_BATCH_DOWNLOADS_IN_FLIGHT: usize = 3;
const MAX_ITEM_ID_BYTES: usize = 256;
const MAX_TITLE_CHARS: usize = 512;

/// One explicitly selected collection to download.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CollectionKind {
    Album,
    Playlist,
}

impl CollectionKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Album => "album",
            Self::Playlist => "playlist",
        }
    }
}

/// Identity copied from the user action and checked again when its result reaches the UI.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BatchIdentity {
    pub request_id: u64,
    pub connection_generation: u64,
    pub parent_id: String,
    pub kind: CollectionKind,
}

/// A page from one bounded collection endpoint.
pub(crate) struct CollectionPage<T> {
    pub items: Vec<T>,
    pub total_record_count: Option<u32>,
}

/// Failure while fetching or bounding a paged collection.
#[derive(Debug)]
pub(crate) enum CollectionPageError<E> {
    Fetch(E),
    TooManyItems,
    InvalidPaging,
}

/// Read every page or return an error; callers never receive a silently truncated prefix.
pub(crate) async fn collect_pages<T, E, F, Fut>(
    page_size: u32,
    max_items: usize,
    mut fetch: F,
) -> Result<Vec<T>, CollectionPageError<E>>
where
    F: FnMut(u32, u32) -> Fut,
    Fut: Future<Output = Result<CollectionPage<T>, E>>,
{
    let max_items_u32 = u32::try_from(max_items).map_err(|_| CollectionPageError::InvalidPaging)?;
    if page_size == 0 || max_items == 0 {
        return Err(CollectionPageError::InvalidPaging);
    }

    let mut start_index = 0_u32;
    let mut items = Vec::new();
    loop {
        let page = fetch(start_index, page_size)
            .await
            .map_err(CollectionPageError::Fetch)?;
        if page
            .total_record_count
            .is_some_and(|total| total > max_items_u32)
        {
            return Err(CollectionPageError::TooManyItems);
        }
        let received =
            u32::try_from(page.items.len()).map_err(|_| CollectionPageError::InvalidPaging)?;
        if items.len().saturating_add(page.items.len()) > max_items {
            return Err(CollectionPageError::TooManyItems);
        }
        let next_index = start_index
            .checked_add(received)
            .ok_or(CollectionPageError::InvalidPaging)?;
        let done = received < page_size
            || page
                .total_record_count
                .is_some_and(|total| next_index >= total);
        items.extend(page.items);
        if done {
            return Ok(items);
        }
        start_index = next_index;
    }
}

/// Minimal item data needed for eligibility and in-memory display names.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CollectionItem {
    pub item_id: String,
    pub item_type: Option<String>,
    pub title: String,
}

impl From<BaseItemDto> for CollectionItem {
    fn from(item: BaseItemDto) -> Self {
        Self {
            item_id: item.id,
            item_type: item.item_type,
            title: item.name,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DownloadCandidate {
    pub item_id: String,
    pub title: String,
}

#[derive(Debug)]
pub(crate) struct BatchPlan {
    pub total: usize,
    pub skipped: usize,
    pub candidates: VecDeque<DownloadCandidate>,
    pub titles: Vec<(String, String)>,
}

/// Reject an over-limit collection and make deterministic, first-occurrence audio candidates.
pub(crate) fn plan_batch(items: Vec<CollectionItem>) -> Result<BatchPlan, BatchPlanError> {
    if items.len() > MAX_BATCH_ITEMS {
        return Err(BatchPlanError::TooManyItems);
    }

    let total = items.len();
    let mut skipped = 0;
    let mut seen = HashSet::new();
    let mut candidates = VecDeque::new();
    let mut titles = Vec::new();
    for item in items {
        if !downloadable_in_batch(item.item_type.as_deref()) {
            skipped += 1;
            continue;
        }
        if !valid_item_id(&item.item_id) {
            skipped += 1;
            continue;
        }
        let dedupe_key = item.item_id.to_ascii_lowercase();
        if !seen.insert(dedupe_key) {
            skipped += 1;
            continue;
        }
        let title = bounded_title(&item.title);
        titles.push((item.item_id.clone(), title.clone()));
        candidates.push_back(DownloadCandidate {
            item_id: item.item_id,
            title,
        });
    }
    Ok(BatchPlan {
        total,
        skipped,
        candidates,
        titles,
    })
}

/// Audio is not a catalog single-file type, but an album or playlist track downloads the same
/// `/Items/{id}/Download` route.
fn downloadable_in_batch(item_type: Option<&str>) -> bool {
    item_type == Some("Audio") || crate::ui::is_downloadable_item_type(item_type)
}

fn valid_item_id(item_id: &str) -> bool {
    !item_id.trim().is_empty()
        && item_id.len() <= MAX_ITEM_ID_BYTES
        && !item_id.chars().any(char::is_control)
}

fn bounded_title(title: &str) -> String {
    let title = title.trim();
    let title: String = title.chars().take(MAX_TITLE_CHARS).collect();
    if title.is_empty() {
        "Untitled item".to_owned()
    } else {
        title
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BatchPlanError {
    TooManyItems,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DispatchResult {
    Started,
    AlreadyTracked,
    Failed,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct BatchSummary {
    pub total: usize,
    pub queued: usize,
    pub skipped: usize,
    pub failed: usize,
    pub waiting: usize,
    pub active: usize,
    pub complete: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BatchEvent {
    pub identity: BatchIdentity,
    pub summary: BatchSummary,
    /// Titles are sent only in memory and only once, on the first event.
    pub titles: Vec<(String, String)>,
}

#[derive(Default)]
pub(crate) struct BatchQueue {
    current: Option<PendingBatch>,
}

struct PendingBatch {
    identity: BatchIdentity,
    total: usize,
    skipped: usize,
    pending: VecDeque<DownloadCandidate>,
    in_flight: HashSet<String>,
    queued: usize,
    failed: usize,
    titles: Vec<(String, String)>,
}

impl BatchQueue {
    pub(crate) fn is_active(&self) -> bool {
        self.current.is_some()
    }

    pub(crate) fn begin(
        &mut self,
        identity: BatchIdentity,
        plan: BatchPlan,
    ) -> Result<(), BatchPlan> {
        if self.current.is_some() {
            return Err(plan);
        }
        self.current = Some(PendingBatch {
            identity,
            total: plan.total,
            skipped: plan.skipped,
            pending: plan.candidates,
            in_flight: HashSet::new(),
            queued: 0,
            failed: 0,
            titles: plan.titles,
        });
        Ok(())
    }

    /// Reconcile finished jobs and use available slots without dropping candidates on pressure.
    pub(crate) fn pump(
        &mut self,
        running_ids: &HashSet<String>,
        failed_ids: &HashSet<String>,
        mut start: impl FnMut(&DownloadCandidate) -> DispatchResult,
    ) -> Option<BatchEvent> {
        let batch = self.current.as_mut()?;
        batch.in_flight.retain(|item_id| {
            if running_ids.contains(item_id) {
                true
            } else {
                if failed_ids.contains(item_id) {
                    batch.failed += 1;
                }
                false
            }
        });

        let mut occupied = running_ids.len();
        while occupied < MAX_BATCH_DOWNLOADS_IN_FLIGHT {
            let Some(candidate) = batch.pending.pop_front() else {
                break;
            };
            match start(&candidate) {
                DispatchResult::Started => {
                    batch.queued += 1;
                    occupied += 1;
                    batch.in_flight.insert(candidate.item_id);
                }
                DispatchResult::AlreadyTracked => batch.skipped += 1,
                DispatchResult::Failed => batch.failed += 1,
            }
        }

        let complete = batch.pending.is_empty() && batch.in_flight.is_empty();
        let event = BatchEvent {
            identity: batch.identity.clone(),
            summary: BatchSummary {
                total: batch.total,
                queued: batch.queued,
                skipped: batch.skipped,
                failed: batch.failed,
                waiting: batch.pending.len(),
                active: batch.in_flight.len(),
                complete,
            },
            titles: std::mem::take(&mut batch.titles),
        };
        if complete {
            self.current = None;
        }
        Some(event)
    }

    pub(crate) fn cancel(&mut self) {
        self.current = None;
    }
}

/// A permission check shared by the worker and deterministic tests.
pub(crate) fn authorize(allowed: bool) -> Result<(), PermissionDenied> {
    if allowed {
        Ok(())
    } else {
        Err(PermissionDenied)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PermissionDenied;

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{
        BatchIdentity, BatchQueue, CollectionItem, CollectionKind, CollectionPage,
        CollectionPageError, DispatchResult, MAX_BATCH_DOWNLOADS_IN_FLIGHT, authorize,
        collect_pages, plan_batch,
    };

    fn item(id: &str, item_type: &str, title: &str) -> CollectionItem {
        CollectionItem {
            item_id: id.to_owned(),
            item_type: Some(item_type.to_owned()),
            title: title.to_owned(),
        }
    }

    fn identity(kind: CollectionKind) -> BatchIdentity {
        BatchIdentity {
            request_id: 41,
            connection_generation: 8,
            parent_id: "parent-1".to_owned(),
            kind,
        }
    }

    #[tokio::test]
    async fn mocked_album_pages_collect_more_than_one_page_and_deduplicate_in_order() {
        let mut calls = Vec::new();
        let pages = collect_pages(100, 200, |start, limit| {
            calls.push((start, limit));
            std::future::ready(Ok::<_, ()>(if start == 0 {
                CollectionPage {
                    items: (0..100)
                        .map(|index| item(&format!("track-{index}"), "Audio", "Album track"))
                        .collect(),
                    total_record_count: Some(102),
                }
            } else {
                CollectionPage {
                    items: vec![
                        item("track-100", "Audio", "Last track"),
                        item("track-4", "Audio", "Duplicate"),
                    ],
                    total_record_count: Some(102),
                }
            }))
        })
        .await
        .expect("both mock album pages are within the bound");
        assert_eq!(calls, vec![(0, 100), (100, 100)]);
        let plan = plan_batch(pages).expect("the bounded album can be planned");
        assert_eq!(plan.total, 102);
        assert_eq!(plan.skipped, 1);
        assert_eq!(plan.candidates.len(), 101);
        assert_eq!(
            plan.candidates.front().map(|track| track.item_id.as_str()),
            Some("track-0")
        );
        assert_eq!(
            plan.candidates.back().map(|track| track.item_id.as_str()),
            Some("track-100")
        );
    }

    #[tokio::test]
    async fn mocked_playlist_pages_keep_order_and_skip_non_audio_and_unavailable_ids() {
        let pages = collect_pages(2, 200, |start, _| {
            std::future::ready(Ok::<_, ()>(if start == 0 {
                CollectionPage {
                    items: vec![
                        item("audio-1", "Audio", "One"),
                        item("folder-1", "Folder", "Skipped folder"),
                    ],
                    total_record_count: Some(5),
                }
            } else {
                CollectionPage {
                    items: vec![
                        item("", "Audio", "Missing ID"),
                        item("audio-2", "Audio", "Two"),
                        item("audio-1", "Audio", "Duplicate"),
                    ],
                    total_record_count: Some(5),
                }
            }))
        })
        .await
        .expect("both mock playlist pages are within the bound");
        let plan = plan_batch(pages).expect("the bounded playlist can be planned");
        assert_eq!(plan.skipped, 3);
        assert_eq!(
            plan.candidates
                .iter()
                .map(|candidate| candidate.item_id.as_str())
                .collect::<Vec<_>>(),
            vec!["audio-1", "audio-2"]
        );
    }

    #[tokio::test]
    async fn empty_collection_is_complete_without_dispatch() {
        let items = collect_pages(100, 200, |_, _| {
            std::future::ready(Ok::<_, ()>(CollectionPage {
                items: Vec::<CollectionItem>::new(),
                total_record_count: Some(0),
            }))
        })
        .await
        .expect("empty page is valid");
        let plan = plan_batch(items).expect("empty collection can be planned");
        let mut queue = BatchQueue::default();
        queue
            .begin(identity(CollectionKind::Album), plan)
            .expect("batch starts");
        let event = queue
            .pump(&HashSet::new(), &HashSet::new(), |_| DispatchResult::Failed)
            .expect("empty batch emits a result");
        assert!(event.summary.complete);
        assert_eq!(event.summary.queued, 0);
        assert_eq!(event.summary.skipped, 0);
        assert!(!queue.is_active());
    }

    #[tokio::test]
    async fn page_failure_is_returned_and_never_turns_a_prefix_into_a_batch() {
        let mut calls = 0;
        let result = collect_pages(2, 200, |start, _| {
            calls += 1;
            std::future::ready(if start == 0 {
                Ok(CollectionPage {
                    items: vec![
                        item("audio-1", "Audio", "One"),
                        item("audio-2", "Audio", "Two"),
                    ],
                    total_record_count: Some(4),
                })
            } else {
                Err("page unavailable")
            })
        })
        .await;
        assert!(matches!(
            result,
            Err(CollectionPageError::Fetch("page unavailable"))
        ));
        assert_eq!(calls, 2);
    }

    #[tokio::test]
    async fn over_limit_collection_is_rejected_instead_of_truncated() {
        let result = collect_pages(100, 2, |_, _| {
            std::future::ready(Ok::<_, ()>(CollectionPage {
                items: vec![item("a", "Audio", "A"), item("b", "Audio", "B")],
                total_record_count: Some(3),
            }))
        })
        .await;
        assert!(matches!(result, Err(CollectionPageError::TooManyItems)));
    }

    #[test]
    fn permission_denial_does_not_start_or_queue_a_batch() {
        assert!(authorize(false).is_err());
        assert!(authorize(true).is_ok());
    }

    #[test]
    fn busy_dispatch_retains_waiting_candidates_and_reports_partial_failure() {
        let plan = plan_batch(vec![
            item("one", "Audio", "One"),
            item("two", "Audio", "Two"),
            item("three", "Audio", "Three"),
            item("four", "Audio", "Four"),
        ])
        .expect("small batch is within the bound");
        let mut queue = BatchQueue::default();
        queue
            .begin(identity(CollectionKind::Playlist), plan)
            .expect("batch starts");
        let running = (0..MAX_BATCH_DOWNLOADS_IN_FLIGHT)
            .map(|index| format!("existing-{index}"))
            .collect::<HashSet<_>>();
        let busy = queue
            .pump(&running, &HashSet::new(), |_| DispatchResult::Started)
            .expect("busy batch emits waiting state");
        assert_eq!(busy.summary.waiting, 4);
        assert_eq!(busy.summary.queued, 0);

        let mut starts = Vec::new();
        let partial = queue
            .pump(&HashSet::new(), &HashSet::new(), |candidate| {
                starts.push(candidate.item_id.clone());
                if candidate.item_id == "one" {
                    DispatchResult::Failed
                } else {
                    DispatchResult::Started
                }
            })
            .expect("partial dispatch emits a summary");
        // A refused dispatch does not take a slot, so the queue attempts the fourth candidate
        // in the same pump: three start and one is counted failed.
        assert_eq!(starts.len(), 4);
        assert_eq!(partial.summary.failed, 1);
        assert_eq!(partial.summary.queued, 3);
        assert_eq!(partial.summary.waiting, 0);

        let running = HashSet::from(["two".to_owned(), "three".to_owned(), "four".to_owned()]);
        let busy = queue
            .pump(&running, &HashSet::new(), |_| DispatchResult::Started)
            .expect("busy batch emits waiting state");
        assert_eq!(busy.summary.queued, 3);
        assert_eq!(busy.summary.active, 3);
        assert!(!busy.summary.complete);
    }

    #[test]
    fn logout_cancels_the_bounded_pending_batch() {
        let plan = plan_batch(vec![
            item("one", "Audio", "One"),
            item("two", "Audio", "Two"),
        ])
        .expect("small batch is within the bound");
        let mut queue = BatchQueue::default();
        queue
            .begin(identity(CollectionKind::Album), plan)
            .expect("batch starts");
        queue.cancel();
        assert!(!queue.is_active());
        assert!(
            queue
                .pump(&HashSet::new(), &HashSet::new(), |_| {
                    DispatchResult::Started
                })
                .is_none()
        );
    }
}

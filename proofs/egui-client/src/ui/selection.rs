//! Multi-select state for one loaded catalog page.
//!
//! The grid hands every Ctrl/Space toggle and Shift extension to this model instead of
//! touching the app's item list, so the rules stay testable without egui. A scope string
//! identifies the page and library the selection belongs to; a different scope drops the
//! selection rather than letting ids from another page linger in the toolbar.

use std::collections::{BTreeSet, HashSet};

/// Selected catalog item ids plus the anchor a Shift click extends from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct LibrarySelection {
    /// Page/library key this selection was made under. `None` until the grid binds one.
    scope: Option<String>,
    selected: BTreeSet<String>,
    /// Index in the loaded item list of the last card toggled, for Shift extension.
    anchor: Option<usize>,
}

impl LibrarySelection {
    /// Whether the selection toolbar should be visible.
    pub(crate) fn is_active(&self) -> bool {
        !self.selected.is_empty()
    }

    pub(crate) fn len(&self) -> usize {
        self.selected.len()
    }

    pub(crate) fn contains(&self, item_id: &str) -> bool {
        self.selected.contains(item_id)
    }

    pub(crate) fn selected_ids(&self) -> impl Iterator<Item = &str> {
        self.selected.iter().map(String::as_str)
    }

    /// Binds the model to a page/library. A changed scope clears the selection, which keeps
    /// ids from a previous layer out of a toolbar drawn on the new one.
    pub(crate) fn bind_scope(&mut self, scope: &str) {
        if self.scope.as_deref() != Some(scope) {
            self.scope = Some(scope.to_owned());
            self.clear();
        }
    }

    /// Ctrl+click or Space: add the item, or remove it when it was already selected.
    pub(crate) fn toggle(&mut self, item_id: &str, index: usize) {
        if !self.selected.remove(item_id) {
            self.selected.insert(item_id.to_owned());
        }
        if self.selected.is_empty() {
            self.anchor = None;
        } else {
            self.anchor = Some(index);
        }
    }

    /// Shift+click: select every loaded item between the anchor and `index`, inclusive.
    /// Without an anchor the click behaves like a plain toggle.
    pub(crate) fn extend<'a>(&mut self, item_ids: impl IntoIterator<Item = &'a str>, index: usize) {
        let item_ids: Vec<&str> = item_ids.into_iter().collect();
        let Some(anchor) = self.anchor else {
            if let Some(item_id) = item_ids.get(index) {
                self.selected.insert((*item_id).to_owned());
                self.anchor = Some(index);
            }
            return;
        };
        if anchor >= item_ids.len() || index >= item_ids.len() {
            return;
        }
        let (first, last) = if anchor <= index {
            (anchor, index)
        } else {
            (index, anchor)
        };
        for item_id in &item_ids[first..=last] {
            self.selected.insert((*item_id).to_owned());
        }
    }

    /// Ctrl+A: select every item currently loaded for the grid.
    pub(crate) fn select_all<'a>(&mut self, item_ids: impl IntoIterator<Item = &'a str>) {
        self.selected = item_ids.into_iter().map(str::to_owned).collect();
        self.anchor = if self.selected.is_empty() {
            None
        } else {
            Some(0)
        };
    }

    /// Drops ids that the freshly loaded page no longer contains. Auto-prefetch appends pages
    /// while the user selects, so stale ids are pruned rather than keeping the toolbar count.
    pub(crate) fn retain_loaded<'a>(&mut self, item_ids: impl IntoIterator<Item = &'a str>) {
        let loaded: HashSet<&str> = item_ids.into_iter().collect();
        self.selected
            .retain(|item_id| loaded.contains(item_id.as_str()));
        if self.selected.is_empty() {
            self.anchor = None;
        }
    }

    /// Leaves selection mode (Clear, Esc, or a page change).
    pub(crate) fn clear(&mut self) {
        self.selected.clear();
        self.anchor = None;
    }
}

#[cfg(test)]
mod tests {
    use super::LibrarySelection;

    #[test]
    fn toggle_adds_and_removes_the_same_item() {
        let mut selection = LibrarySelection::default();
        assert!(!selection.is_active());

        selection.toggle("movie-a", 2);
        assert!(selection.is_active());
        assert_eq!(selection.len(), 1);
        assert!(selection.contains("movie-a"));

        selection.toggle("movie-b", 3);
        assert_eq!(selection.len(), 2);

        selection.toggle("movie-a", 2);
        assert_eq!(selection.len(), 1);
        assert!(!selection.contains("movie-a"));

        selection.toggle("movie-b", 3);
        assert!(!selection.is_active());
        assert_eq!(selection.len(), 0);
    }

    #[test]
    fn shift_extends_both_ways_from_the_anchor() {
        let mut selection = LibrarySelection::default();
        selection.toggle("b", 1);
        selection.extend(["a", "b", "c", "d"], 3);
        assert_eq!(
            selection.selected_ids().collect::<Vec<_>>(),
            ["b", "c", "d"]
        );

        selection.extend(["a", "b", "c", "d"], 0);
        assert_eq!(
            selection.selected_ids().collect::<Vec<_>>(),
            ["a", "b", "c", "d"]
        );
    }

    #[test]
    fn shift_without_an_anchor_selects_only_the_clicked_item() {
        let mut selection = LibrarySelection::default();
        selection.extend(["a", "b", "c"], 1);
        assert_eq!(selection.selected_ids().collect::<Vec<_>>(), ["b"]);
    }

    #[test]
    fn select_all_covers_the_loaded_list() {
        let mut selection = LibrarySelection::default();
        selection.toggle("a", 0);
        selection.select_all(["a", "b", "c"]);
        assert_eq!(selection.len(), 3);
        assert_eq!(
            selection.selected_ids().collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
    }

    #[test]
    fn clear_resets_the_selection() {
        let mut selection = LibrarySelection::default();
        selection.toggle("a", 0);
        selection.toggle("b", 1);
        selection.clear();
        assert!(!selection.is_active());
        assert_eq!(selection.len(), 0);
        assert!(!selection.contains("a"));
    }

    #[test]
    fn a_new_page_scope_clears_the_old_selection() {
        let mut selection = LibrarySelection::default();
        selection.bind_scope("library:movies");
        selection.toggle("movie-a", 0);
        assert!(selection.is_active());

        selection.bind_scope("library:series");
        assert!(!selection.is_active());
        assert!(!selection.contains("movie-a"));

        // Binding the same page again keeps what the user picked there.
        selection.toggle("show-a", 0);
        selection.bind_scope("library:series");
        assert!(selection.contains("show-a"));
    }

    #[test]
    fn retain_loaded_drops_ids_missing_from_the_loaded_page() {
        let mut selection = LibrarySelection::default();
        selection.toggle("a", 0);
        selection.toggle("b", 1);
        selection.retain_loaded(["a", "c"]);
        assert_eq!(selection.selected_ids().collect::<Vec<_>>(), ["a"]);
    }
}

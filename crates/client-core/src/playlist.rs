//! Pure playlist model: ordered entries, entry-ID removal, one-step reordering,
//! and name and ID validation. Performs no I/O.
//!
//! Duplicate policy: the same library item may appear more than once. Each
//! occurrence has its own entry ID, and removal and reordering address entries
//! by entry ID, so only the occurrence the user selected changes. Entry IDs must
//! be unique within one playlist. Reordering is one step per call, matching the
//! one-move-per-request server calls.

use std::collections::HashSet;

/// Longest playlist name accepted, in Unicode scalar values after trimming.
pub const MAX_NAME_CHARS: usize = 100;

/// Most entries one playlist model holds.
pub const MAX_ENTRIES: usize = 1_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaylistError {
    EmptyName,
    NameTooLong,
    InvalidId,
    TooManyEntries,
    DuplicateEntryId,
    UnknownEntry,
}

/// One playlist occurrence: the entry ID used for removal and reordering,
/// and the library item ID it refers to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaylistEntry {
    entry_id: String,
    item_id: String,
}

impl PlaylistEntry {
    /// Build an entry, validating both IDs.
    pub fn new(entry_id: &str, item_id: &str) -> Result<Self, PlaylistError> {
        validate_id(entry_id)?;
        validate_id(item_id)?;
        Ok(Self {
            entry_id: entry_id.to_owned(),
            item_id: item_id.to_owned(),
        })
    }

    pub fn entry_id(&self) -> &str {
        &self.entry_id
    }

    pub fn item_id(&self) -> &str {
        &self.item_id
    }
}

/// Result of a reorder request. `Unchanged` covers a move at either end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoveOutcome {
    Moved { from: usize, to: usize },
    Unchanged { index: usize },
}

/// Ordered entries of one playlist, bounded by [`MAX_ENTRIES`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlaylistModel {
    entries: Vec<PlaylistEntry>,
}

impl PlaylistModel {
    /// Build a model from entries in server order. Fails if there are more than
    /// [`MAX_ENTRIES`] entries or two entries share an entry ID.
    pub fn from_entries(entries: Vec<PlaylistEntry>) -> Result<Self, PlaylistError> {
        if entries.len() > MAX_ENTRIES {
            return Err(PlaylistError::TooManyEntries);
        }
        let mut seen = HashSet::with_capacity(entries.len());
        for entry in &entries {
            if !seen.insert(entry.entry_id()) {
                return Err(PlaylistError::DuplicateEntryId);
            }
        }
        Ok(Self { entries })
    }

    pub fn entries(&self) -> &[PlaylistEntry] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Index of the entry with this entry ID, or `None`.
    pub fn position(&self, entry_id: &str) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| entry.entry_id() == entry_id)
    }

    /// Append an entry at the end and return its index. Duplicate item IDs are
    /// allowed; the entry ID must be new.
    pub fn append(&mut self, entry: PlaylistEntry) -> Result<usize, PlaylistError> {
        if self.entries.len() >= MAX_ENTRIES {
            return Err(PlaylistError::TooManyEntries);
        }
        if self.position(entry.entry_id()).is_some() {
            return Err(PlaylistError::DuplicateEntryId);
        }
        self.entries.push(entry);
        Ok(self.entries.len() - 1)
    }

    /// Remove the entry with this entry ID and return it. Other occurrences of
    /// the same item stay in place.
    pub fn remove(&mut self, entry_id: &str) -> Result<PlaylistEntry, PlaylistError> {
        let index = self.position(entry_id).ok_or(PlaylistError::UnknownEntry)?;
        Ok(self.entries.remove(index))
    }

    /// Move the entry one position earlier. At the first position this is a no-op.
    pub fn move_up(&mut self, entry_id: &str) -> Result<MoveOutcome, PlaylistError> {
        let index = self.position(entry_id).ok_or(PlaylistError::UnknownEntry)?;
        if index == 0 {
            return Ok(MoveOutcome::Unchanged { index });
        }
        self.entries.swap(index, index - 1);
        Ok(MoveOutcome::Moved {
            from: index,
            to: index - 1,
        })
    }

    /// Move the entry one position later. At the last position this is a no-op.
    pub fn move_down(&mut self, entry_id: &str) -> Result<MoveOutcome, PlaylistError> {
        let index = self.position(entry_id).ok_or(PlaylistError::UnknownEntry)?;
        if index + 1 >= self.entries.len() {
            return Ok(MoveOutcome::Unchanged { index });
        }
        self.entries.swap(index, index + 1);
        Ok(MoveOutcome::Moved {
            from: index,
            to: index + 1,
        })
    }
}

/// Validate a playlist name and return it trimmed. Names must be 1 to
/// [`MAX_NAME_CHARS`] characters after trimming.
pub fn validate_name(name: &str) -> Result<&str, PlaylistError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(PlaylistError::EmptyName);
    }
    if trimmed.chars().count() > MAX_NAME_CHARS {
        return Err(PlaylistError::NameTooLong);
    }
    Ok(trimmed)
}

/// Accept a Jellyfin ID: 32 hex digits, or the hyphenated 8-4-4-4-12 form.
/// Anything else is rejected before it reaches a URL or query.
pub fn validate_id(value: &str) -> Result<(), PlaylistError> {
    let is_plain = value.len() == 32 && value.chars().all(|c| c.is_ascii_hexdigit());
    let is_hyphenated = value.len() == 36
        && value.char_indices().all(|(index, c)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                c == '-'
            } else {
                c.is_ascii_hexdigit()
            }
        });
    if is_plain || is_hyphenated {
        Ok(())
    } else {
        Err(PlaylistError::InvalidId)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_ENTRIES, MAX_NAME_CHARS, MoveOutcome, PlaylistEntry, PlaylistError, PlaylistModel,
        validate_id, validate_name,
    };

    const ITEM_A: &str = "0000000000000000000000000000aaaa";
    const ITEM_B: &str = "0000000000000000000000000000bbbb";

    fn entry(n: usize, item: &str) -> PlaylistEntry {
        PlaylistEntry::new(&format!("{:032x}", 0x1000 + n), item).expect("valid entry")
    }

    fn model_of(items: &[&str]) -> PlaylistModel {
        let mut model = PlaylistModel::default();
        for (n, item) in items.iter().enumerate() {
            model.append(entry(n, item)).expect("append");
        }
        model
    }

    fn entry_id(n: usize) -> String {
        format!("{:032x}", 0x1000 + n)
    }

    #[test]
    fn move_up_at_first_position_is_a_no_op() {
        let mut model = model_of(&[ITEM_A, ITEM_B]);
        let before = model.clone();
        assert_eq!(
            model.move_up(&entry_id(0)),
            Ok(MoveOutcome::Unchanged { index: 0 })
        );
        assert_eq!(model, before);
    }

    #[test]
    fn move_down_at_last_position_is_a_no_op() {
        let mut model = model_of(&[ITEM_A, ITEM_B]);
        let before = model.clone();
        assert_eq!(
            model.move_down(&entry_id(1)),
            Ok(MoveOutcome::Unchanged { index: 1 })
        );
        assert_eq!(model, before);
    }

    #[test]
    fn move_up_and_down_swap_one_step_and_report_the_move() {
        let mut model = model_of(&[ITEM_A, ITEM_B, ITEM_A]);
        assert_eq!(
            model.move_up(&entry_id(2)),
            Ok(MoveOutcome::Moved { from: 2, to: 1 })
        );
        assert_eq!(model.position(&entry_id(2)), Some(1));
        assert_eq!(
            model.move_down(&entry_id(2)),
            Ok(MoveOutcome::Moved { from: 1, to: 2 })
        );
        assert_eq!(model.position(&entry_id(2)), Some(2));
    }

    #[test]
    fn move_of_unknown_entry_is_an_error() {
        let mut model = model_of(&[ITEM_A]);
        assert_eq!(
            model.move_up(&entry_id(9)),
            Err(PlaylistError::UnknownEntry)
        );
        assert_eq!(
            model.move_down(&entry_id(9)),
            Err(PlaylistError::UnknownEntry)
        );
    }

    #[test]
    fn remove_by_entry_id_keeps_other_occurrences_of_the_same_item() {
        let mut model = model_of(&[ITEM_A, ITEM_B, ITEM_A]);
        let removed = model
            .remove(&entry_id(2))
            .expect("remove second occurrence");
        assert_eq!(removed.item_id(), ITEM_A);
        assert_eq!(removed.entry_id(), entry_id(2));
        assert_eq!(model.len(), 2);
        assert_eq!(model.entries()[0].entry_id(), entry_id(0));
        assert_eq!(model.entries()[0].item_id(), ITEM_A);
        assert_eq!(model.entries()[1].item_id(), ITEM_B);
    }

    #[test]
    fn remove_unknown_entry_is_an_error_and_leaves_model_unchanged() {
        let mut model = model_of(&[ITEM_A]);
        let before = model.clone();
        assert_eq!(model.remove(&entry_id(7)), Err(PlaylistError::UnknownEntry));
        assert_eq!(model, before);
    }

    #[test]
    fn duplicate_items_are_allowed_but_duplicate_entry_ids_are_not() {
        let mut model = model_of(&[ITEM_A]);
        assert_eq!(model.append(entry(5, ITEM_A)), Ok(1));
        assert_eq!(
            model.append(entry(5, ITEM_B)),
            Err(PlaylistError::DuplicateEntryId)
        );
        assert_eq!(
            PlaylistModel::from_entries(vec![entry(1, ITEM_A), entry(1, ITEM_B)]),
            Err(PlaylistError::DuplicateEntryId)
        );
    }

    #[test]
    fn length_is_bounded_on_construction_and_append() {
        let too_many: Vec<PlaylistEntry> = (0..=MAX_ENTRIES).map(|n| entry(n, ITEM_A)).collect();
        assert_eq!(
            PlaylistModel::from_entries(too_many),
            Err(PlaylistError::TooManyEntries)
        );
        let full: Vec<PlaylistEntry> = (0..MAX_ENTRIES).map(|n| entry(n, ITEM_A)).collect();
        let mut model = PlaylistModel::from_entries(full).expect("at the bound");
        assert_eq!(
            model.append(entry(MAX_ENTRIES, ITEM_B)),
            Err(PlaylistError::TooManyEntries)
        );
        assert_eq!(model.len(), MAX_ENTRIES);
    }

    #[test]
    fn name_must_be_one_to_one_hundred_characters_after_trimming() {
        assert_eq!(validate_name("  Road trip  "), Ok("Road trip"));
        assert_eq!(validate_name(""), Err(PlaylistError::EmptyName));
        assert_eq!(validate_name("   \t "), Err(PlaylistError::EmptyName));
        let longest = "é".repeat(MAX_NAME_CHARS);
        assert_eq!(validate_name(&longest), Ok(longest.as_str()));
        let too_long = "é".repeat(MAX_NAME_CHARS + 1);
        assert_eq!(validate_name(&too_long), Err(PlaylistError::NameTooLong));
    }

    #[test]
    fn ids_accept_plain_and_hyphenated_guids_and_reject_other_text() {
        assert_eq!(validate_id(ITEM_A), Ok(()));
        assert_eq!(validate_id("01234567-89ab-cdef-0123-456789abcdef"), Ok(()));
        assert_eq!(validate_id(""), Err(PlaylistError::InvalidId));
        assert_eq!(validate_id("abc,def"), Err(PlaylistError::InvalidId));
        assert_eq!(validate_id("zz"), Err(PlaylistError::InvalidId));
        assert_eq!(
            validate_id("01234567-89ab-cdef-0123-456789abcdeg"),
            Err(PlaylistError::InvalidId)
        );
        assert_eq!(
            validate_id("0123456789abcdef0123456789abcdef0"),
            Err(PlaylistError::InvalidId)
        );
    }

    #[test]
    fn entry_constructor_validates_both_ids() {
        assert!(PlaylistEntry::new("not-an-id", ITEM_A).is_err());
        assert!(PlaylistEntry::new(&entry_id(1), "bad id").is_err());
        assert!(PlaylistEntry::new(&entry_id(1), ITEM_A).is_ok());
    }
}

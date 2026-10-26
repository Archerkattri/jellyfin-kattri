//! Pure music queue model: ordered items, a current position, a seeded shuffle
//! order, repeat modes, and the next, previous, and end-of-track rules. Performs
//! no I/O and starts no playback. Callers map the returned [`Step`] to player
//! commands.
//!
//! Two orders are kept. `items` is the queue in list order. `play_order` is the
//! sequence playback follows: list order while shuffle is off, a permutation of
//! the list while it is on. The cursor stores a play position, not an item index.
//!
//! Shuffle is regenerated when the queue changes, with the current track fixed
//! at the front. A `play_next` insertion stays immediately after the current
//! track while the remaining tracks receive a new shuffled order.
//! - A user "next" ignores repeat `One`, so the user can leave a repeated item.
//!   At the end of the play order, `Off` and `One` stop: the cursor stays on the
//!   last item and the step is [`Step::End`].
//! - With no current item, `next` and `previous` start at the first item.
//! - Removing the current item keeps it playing ("detached") until it ends. Its
//!   successor is whatever now sits in its slot.
//! - `previous` restarts the current item when more than
//!   [`RESTART_THRESHOLD`] has played, or when there is no earlier item and
//!   repeat is not `All`.
//! - Repeat `One` retries a failed item once, then moves on. Repeat `Off` and
//!   `All` move on at once.
//! - `add_album_and_play` appends the album and starts at its first track.

use std::time::Duration;

use jellyfin_api::session_reporting::{PlaybackOrder, QueueItem, RepeatMode};
use kattri_guard::is_secret_like_key;

/// Most items one queue holds.
pub const MAX_QUEUE_LEN: usize = 1_000;

/// Elapsed time after which `previous` restarts the current item instead of
/// going to the prior track. Exactly this long still goes to the prior track.
pub const RESTART_THRESHOLD: Duration = Duration::from_secs(3);

const DEFAULT_SEED: u64 = 0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueError {
    /// Adding would exceed [`MAX_QUEUE_LEN`].
    Full,
    /// An index is outside the queue.
    OutOfRange,
    /// An album with no tracks was given.
    NoItems,
}

/// What playback should do after a queue operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Play the item at this index in list order.
    Changed(usize),
    /// Play the current item again from the start.
    Restart,
    /// No item follows. The cursor stays on the last item.
    End,
    /// The queue is empty.
    Empty,
}

/// Where playback is. `play` is a play position. `removed` holds the item that
/// was current when it was removed from the queue; it keeps playing until it
/// ends, and `play` then names its successor.
#[derive(Clone, PartialEq, Eq)]
struct Cursor {
    play: usize,
    removed: Option<QueueItem>,
}

/// Small deterministic generator for shuffle. Not for security.
#[derive(Clone)]
struct SplitMix64(u64);

impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

fn shuffle_in_place(values: &mut [usize], rng: &mut SplitMix64) {
    for i in (1..values.len()).rev() {
        let bound = (i as u64) + 1;
        let j = (rng.next_u64() % bound) as usize;
        values.swap(i, j);
    }
}

#[derive(Clone)]
pub struct Queue {
    items: Vec<QueueItem>,
    /// `None` while shuffle is off. `Some` is a permutation of item indices
    /// in play order.
    order: Option<Vec<usize>>,
    repeat: RepeatMode,
    cursor: Option<Cursor>,
    /// True once the current item has been retried after a failure.
    retried: bool,
    rng: SplitMix64,
}

impl Default for Queue {
    fn default() -> Self {
        Self::new()
    }
}

impl Queue {
    /// Empty queue, repeat off, shuffle off.
    pub fn new() -> Self {
        Self::with_seed(DEFAULT_SEED)
    }

    /// Empty queue whose shuffles are fully determined by `seed` and the
    /// sequence of shuffle calls.
    pub fn with_seed(seed: u64) -> Self {
        Self {
            items: Vec::new(),
            order: None,
            repeat: RepeatMode::None,
            cursor: None,
            retried: false,
            rng: SplitMix64(seed),
        }
    }

    /// Items in list order.
    pub fn items(&self) -> &[QueueItem] {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn repeat(&self) -> RepeatMode {
        self.repeat
    }

    pub fn set_repeat(&mut self, mode: RepeatMode) {
        self.repeat = mode;
    }

    pub fn is_shuffled(&self) -> bool {
        self.order.is_some()
    }

    /// Ordering for session reporting: `Shuffle` while shuffle is on.
    pub fn playback_order(&self) -> PlaybackOrder {
        if self.is_shuffled() {
            PlaybackOrder::Shuffle
        } else {
            PlaybackOrder::Default
        }
    }

    /// List index of the current item, or `None` when nothing is current or the
    /// current item was removed.
    pub fn current_index(&self) -> Option<usize> {
        match &self.cursor {
            Some(cursor) if cursor.removed.is_none() => self.item_at(cursor.play),
            _ => None,
        }
    }

    pub fn current(&self) -> Option<&QueueItem> {
        self.current_index().and_then(|index| self.items.get(index))
    }

    /// Remove entries rejected by `accept` while preserving the remaining
    /// playback order and, when possible, the current item.
    ///
    /// Duplicate entries are evaluated independently and retained independently.
    /// If the current entry is rejected (or was already detached), the cursor is
    /// clamped to the last valid play position. No detached item survives
    /// reconciliation. Returns the number of removed entries.
    pub fn retain_items(&mut self, mut accept: impl FnMut(&QueueItem) -> bool) -> usize {
        let accepted: Vec<bool> = self.items.iter().map(&mut accept).collect();
        let removed = accepted.iter().filter(|&&keep| !keep).count();
        let has_detached_current = self
            .cursor
            .as_ref()
            .is_some_and(|cursor| cursor.removed.is_some());
        if removed == 0 && !has_detached_current {
            return 0;
        }

        let mut remap = vec![None; self.items.len()];
        let mut next_index = 0;
        for (index, &keep) in accepted.iter().enumerate() {
            if keep {
                remap[index] = Some(next_index);
                next_index += 1;
            }
        }

        let old_cursor = self.cursor.clone();
        let current_index = old_cursor
            .as_ref()
            .filter(|cursor| cursor.removed.is_none())
            .and_then(|cursor| self.item_at(cursor.play));
        let current_remapped = current_index.and_then(|index| remap[index]);

        let old_order = self.order.take();
        let filtered_order = old_order.as_ref().map(|order| {
            order
                .iter()
                .filter_map(|&index| remap.get(index).copied().flatten())
                .collect::<Vec<_>>()
        });
        self.items = std::mem::take(&mut self.items)
            .into_iter()
            .enumerate()
            .filter_map(|(index, item)| accepted[index].then_some(item))
            .collect();
        self.order = filtered_order;

        self.cursor = match (old_cursor, self.items.len()) {
            (_, 0) => None,
            (Some(cursor), _) => {
                let play = current_remapped
                    .and_then(|index| {
                        self.order.as_ref().map_or(Some(index), |order| {
                            order.iter().position(|&entry| entry == index)
                        })
                    })
                    .unwrap_or_else(|| cursor.play.min(self.items.len() - 1));
                Some(Cursor {
                    play,
                    removed: None,
                })
            }
            (None, _) => None,
        };
        if current_remapped.is_none() {
            self.retried = false;
        }

        removed
    }

    /// The item that was removed while it was playing, until it ends.
    pub fn removed_current(&self) -> Option<&QueueItem> {
        self.cursor
            .as_ref()
            .and_then(|cursor| cursor.removed.as_ref())
    }

    /// List indices in the order playback follows.
    pub fn play_order(&self) -> Vec<usize> {
        (0..self.items.len())
            .filter_map(|play| self.item_at(play))
            .collect()
    }

    /// Append an item to the end of the queue. Current item unchanged.
    pub fn add(&mut self, item: QueueItem) -> Result<(), QueueError> {
        self.ensure_room(1)?;
        self.append(item);
        self.reshuffle_after_change(None);
        Ok(())
    }

    /// Insert an item right after the current item. With no current item it goes
    /// to the front. After a removed current item it takes that item's slot.
    pub fn play_next(&mut self, item: QueueItem) -> Result<(), QueueError> {
        self.ensure_room(1)?;
        let at = match &self.cursor {
            None => 0,
            Some(cursor) if cursor.removed.is_none() => cursor.play + 1,
            Some(cursor) => cursor.play,
        };
        self.insert_at_play(at, item);
        let next = self.item_at(at);
        self.reshuffle_after_change(next);
        Ok(())
    }

    /// Append an album and start at its first track. The album's tracks are
    /// appended in album order.
    pub fn add_album_and_play(&mut self, album: Vec<QueueItem>) -> Result<(), QueueError> {
        if album.is_empty() {
            return Err(QueueError::NoItems);
        }
        self.ensure_room(album.len())?;
        let first = self.items.len();
        for item in album {
            self.append(item);
        }
        self.place(Some(Cursor {
            play: first,
            removed: None,
        }));
        if self.order.is_some() {
            let following: Vec<usize> = (first + 1..self.items.len()).collect();
            self.reshuffle_with_following(&following);
        }
        Ok(())
    }

    /// Make the item at this list index current.
    pub fn select(&mut self, index: usize) -> Result<(), QueueError> {
        let play = self.play_pos_of(index).ok_or(QueueError::OutOfRange)?;
        self.place(Some(Cursor {
            play,
            removed: None,
        }));
        Ok(())
    }

    /// Remove an item by list index and return it. Removing the current item
    /// keeps it playing until it ends.
    pub fn remove(&mut self, index: usize) -> Result<QueueItem, QueueError> {
        let play = self.play_pos_of(index).ok_or(QueueError::OutOfRange)?;
        let item = self.items.remove(index);
        if let Some(order) = &mut self.order {
            order.remove(play);
            for entry in order.iter_mut() {
                if *entry > index {
                    *entry -= 1;
                }
            }
        }
        let detach =
            matches!(&self.cursor, Some(cursor) if cursor.removed.is_none() && cursor.play == play);
        if detach {
            self.cursor = Some(Cursor {
                play,
                removed: Some(item.clone()),
            });
            self.retried = false;
        } else if let Some(cursor) = self.cursor.as_mut().filter(|cursor| play < cursor.play) {
            cursor.play -= 1;
        }
        self.reshuffle_after_change(None);
        Ok(item)
    }

    /// Move the item at list index `from` to list index `to`. The current track
    /// stays current. In shuffled mode the play order is unchanged.
    pub fn move_item(&mut self, from: usize, to: usize) -> Result<(), QueueError> {
        let len = self.items.len();
        if from >= len || to >= len {
            return Err(QueueError::OutOfRange);
        }
        if from == to {
            return Ok(());
        }
        let map = |index: usize| -> usize {
            if index == from {
                to
            } else if from < to && index > from && index <= to {
                index - 1
            } else if to < from && index >= to && index < from {
                index + 1
            } else {
                index
            }
        };
        let item = self.items.remove(from);
        self.items.insert(to, item);
        match &mut self.order {
            Some(order) => {
                for entry in order.iter_mut() {
                    *entry = map(*entry);
                }
            }
            None => {
                if let Some(cursor) = &mut self.cursor {
                    cursor.play = map(cursor.play);
                }
            }
        }
        self.reshuffle_after_change(None);
        Ok(())
    }

    /// Turn shuffle on or off. Turning it on keeps the current item current and
    /// first in the play order, and does not restart it. Turning it off returns to
    /// list order at the current item. Shuffle with fewer than two items is a
    /// no-op.
    pub fn set_shuffle(&mut self, on: bool) {
        if !on {
            self.unshuffle();
            return;
        }
        if self.order.is_some() || self.items.len() < 2 {
            return;
        }
        let current = match &self.cursor {
            Some(cursor) if cursor.removed.is_none() => self.item_at(cursor.play),
            _ => None,
        };
        let mut rest: Vec<usize> = (0..self.items.len())
            .filter(|&index| Some(index) != current)
            .collect();
        shuffle_in_place(&mut rest, &mut self.rng);
        let mut order = Vec::with_capacity(self.items.len());
        if let Some(index) = current {
            order.push(index);
        }
        order.extend(rest);
        if let Some(cursor) = &mut self.cursor {
            cursor.play = 0;
        }
        self.order = Some(order);
    }

    /// User "next": ignores repeat `One`.
    pub fn next_track(&mut self) -> Step {
        self.step_forward()
    }

    /// User "previous". `elapsed` is how long the current item has played.
    pub fn previous_track(&mut self, elapsed: Duration) -> Step {
        let len = self.items.len();
        if len == 0 {
            self.place(None);
            return Step::Empty;
        }
        if self.cursor.is_none() {
            return self.step_forward();
        }
        if elapsed > RESTART_THRESHOLD {
            return Step::Restart;
        }
        let play = self.cursor.as_ref().map_or(0, |cursor| cursor.play);
        let target = if play > 0 {
            Some(play - 1)
        } else if self.repeat == RepeatMode::All {
            Some(len - 1)
        } else {
            None
        };
        match target {
            Some(play) => self.land(play),
            None => Step::Restart,
        }
    }

    /// Automatic advance after the current item finished. Repeat `One` replays
    /// an attached current item. A removed current item moves on to its successor.
    pub fn track_ended(&mut self) -> Step {
        let Some(cursor) = &self.cursor else {
            return Step::End;
        };
        if cursor.removed.is_none() && self.repeat == RepeatMode::One {
            self.retried = false;
            return Step::Restart;
        }
        self.step_forward()
    }

    /// The current item failed to play. Repeat `One` retries it once and then
    /// moves on. Other modes move on at once.
    pub fn track_failed(&mut self) -> Step {
        let Some(cursor) = &self.cursor else {
            return Step::End;
        };
        if cursor.removed.is_none() && self.repeat == RepeatMode::One && !self.retried {
            self.retried = true;
            return Step::Restart;
        }
        self.step_forward()
    }

    fn item_at(&self, play: usize) -> Option<usize> {
        match &self.order {
            None => (play < self.items.len()).then_some(play),
            Some(order) => order.get(play).copied(),
        }
    }

    fn play_pos_of(&self, index: usize) -> Option<usize> {
        match &self.order {
            None => (index < self.items.len()).then_some(index),
            Some(order) => order.iter().position(|&entry| entry == index),
        }
    }

    fn ensure_room(&self, extra: usize) -> Result<(), QueueError> {
        if self.items.len().saturating_add(extra) > MAX_QUEUE_LEN {
            Err(QueueError::Full)
        } else {
            Ok(())
        }
    }

    fn place(&mut self, cursor: Option<Cursor>) {
        self.cursor = cursor;
        self.retried = false;
    }

    /// Make the item at play position `play` current, and return it.
    fn land(&mut self, play: usize) -> Step {
        match self.item_at(play) {
            Some(index) => {
                self.place(Some(Cursor {
                    play,
                    removed: None,
                }));
                Step::Changed(index)
            }
            None => Step::End,
        }
    }

    fn step_forward(&mut self) -> Step {
        let len = self.items.len();
        if len == 0 {
            self.place(None);
            return Step::Empty;
        }
        let target = match &self.cursor {
            None => 0,
            Some(cursor) if cursor.removed.is_none() => cursor.play + 1,
            Some(cursor) => cursor.play,
        };
        if target < len {
            return self.land(target);
        }
        if self.repeat == RepeatMode::All {
            return self.land(0);
        }
        self.place(Some(Cursor {
            play: len - 1,
            removed: None,
        }));
        Step::End
    }

    fn append(&mut self, item: QueueItem) {
        let play = self.items.len();
        self.insert_at_play(play, item);
    }

    /// Insert an item at a play position. The caller guarantees the current
    /// cursor needs no adjustment for this position.
    fn insert_at_play(&mut self, play: usize, item: QueueItem) {
        let play = play.min(self.items.len());
        match &mut self.order {
            None => self.items.insert(play, item),
            Some(order) => {
                order.insert(play, self.items.len());
                self.items.push(item);
            }
        }
    }

    /// Rebuild the shuffled tail after a queue mutation without changing the
    /// item currently playing. `next` is pinned immediately after that item.
    fn reshuffle_after_change(&mut self, next: Option<usize>) {
        if self.order.is_none() {
            return;
        }

        self.reshuffle_with_following(next.as_slice());
    }

    /// Shuffle every item after the current item and any entries that must
    /// remain next in sequence (for example, the rest of a newly played album).
    fn reshuffle_with_following(&mut self, following: &[usize]) {
        let current = self.current_index();
        let following: Vec<usize> = following
            .iter()
            .copied()
            .filter(|index| Some(*index) != current && *index < self.items.len())
            .collect();
        let mut rest: Vec<usize> = (0..self.items.len())
            .filter(|index| Some(*index) != current && !following.contains(index))
            .collect();
        shuffle_in_place(&mut rest, &mut self.rng);

        let mut order = Vec::with_capacity(self.items.len());
        if let Some(current) = current {
            order.push(current);
        }
        order.extend(following);
        order.extend(rest);
        self.order = Some(order);

        if let Some(cursor) = &mut self.cursor {
            cursor.play = 0;
        }
    }

    fn unshuffle(&mut self) {
        let Some(order) = self.order.take() else {
            return;
        };
        let len = self.items.len();
        if let Some(cursor) = &mut self.cursor {
            cursor.play = order.get(cursor.play).copied().unwrap_or(len);
        }
    }
}

/// Schema version written to and required from the persisted queue (M3-02).
pub const QUEUE_SCHEMA_VERSION: u64 = 1;

/// Largest encoded queue accepted, in bytes. Product choice: 200 IDs take under 10 KiB, and this
/// size leaves room for the full [`MAX_QUEUE_LEN`] bound with its order array.
pub const MAX_ENCODED_QUEUE_BYTES: usize = 256 * 1024;

/// Longest item ID accepted.
const MAX_ITEM_ID_BYTES: usize = 128;

/// Deepest JSON nesting accepted. The queue format needs two levels.
const MAX_JSON_DEPTH: usize = 4;

const QUEUE_FIELDS: &[&str] = &[
    "schemaVersion",
    "repeat",
    "shuffle",
    "items",
    "order",
    "cursor",
    "detached",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueCodecError {
    /// Encoded text is larger than [`MAX_ENCODED_QUEUE_BYTES`].
    TooLarge,
    /// More items than [`MAX_QUEUE_LEN`].
    TooManyItems,
    /// The schema version is not [`QUEUE_SCHEMA_VERSION`]. Carries the version found.
    UnknownVersion(u64),
    /// Not the expected shape, an unknown or repeated field, or fields that disagree (for
    /// example, an order that is not a permutation of the items).
    Malformed,
    /// An item ID is empty, too long, outside `[A-Za-z0-9_-]`, or credential-like.
    UnsafeId,
}

/// Item IDs are limited to ASCII letters, digits, `-`, and `_`. Jellyfin IDs are GUIDs, so no real
/// ID is refused. The limit keeps the file free of escapes, URLs, and credential text.
fn check_item_id(id: &str) -> Result<(), QueueCodecError> {
    let plain = !id.is_empty()
        && id.len() <= MAX_ITEM_ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
    if !plain || is_secret_like_key(id) {
        return Err(QueueCodecError::UnsafeId);
    }
    Ok(())
}

fn repeat_name(mode: RepeatMode) -> &'static str {
    match mode {
        RepeatMode::None => "off",
        RepeatMode::One => "one",
        RepeatMode::All => "all",
    }
}

fn repeat_from_name(name: &str) -> Option<RepeatMode> {
    match name {
        "off" => Some(RepeatMode::None),
        "one" => Some(RepeatMode::One),
        "all" => Some(RepeatMode::All),
        _ => None,
    }
}

impl Queue {
    /// Versioned JSON for the queue (M3-02). Stores the item IDs, the shuffle order, the repeat
    /// mode, and the cursor. Stores nothing else: no names, stream URLs, playlist item IDs, or
    /// random-generator state. Decoding restores a queue whose `items`, `play_order`,
    /// `current_index`, `removed_current`, `repeat`, and shuffle state match this one.
    pub fn encode(&self) -> Result<String, QueueCodecError> {
        if self.items.len() > MAX_QUEUE_LEN {
            return Err(QueueCodecError::TooManyItems);
        }
        let mut out = String::new();
        out.push_str("{\"schemaVersion\":");
        out.push_str(&QUEUE_SCHEMA_VERSION.to_string());
        out.push_str(",\"repeat\":\"");
        out.push_str(repeat_name(self.repeat));
        out.push_str("\",\"shuffle\":");
        out.push_str(if self.is_shuffled() { "true" } else { "false" });

        out.push_str(",\"items\":[");
        for (position, item) in self.items.iter().enumerate() {
            check_item_id(&item.id)?;
            if position > 0 {
                out.push(',');
            }
            push_id(&mut out, &item.id);
        }

        out.push_str("],\"order\":[");
        if let Some(order) = &self.order {
            for (position, index) in order.iter().enumerate() {
                if position > 0 {
                    out.push(',');
                }
                out.push_str(&index.to_string());
            }
        }

        out.push_str("],\"cursor\":");
        match &self.cursor {
            Some(cursor) => out.push_str(&cursor.play.to_string()),
            None => out.push_str("null"),
        }

        out.push_str(",\"detached\":");
        match self.removed_current() {
            Some(item) => {
                check_item_id(&item.id)?;
                push_id(&mut out, &item.id);
            }
            None => out.push_str("null"),
        }
        out.push('}');

        if out.len() > MAX_ENCODED_QUEUE_BYTES {
            return Err(QueueCodecError::TooLarge);
        }
        Ok(out)
    }

    /// Restore a queue from [`Queue::encode`] output. Refuses unknown versions, unknown or repeated
    /// fields, oversized input, unsafe IDs, and fields that disagree. The shuffle random generator
    /// starts from its default seed. A removed-current item is restored as detached.
    pub fn decode(text: &str) -> Result<Self, QueueCodecError> {
        if text.len() > MAX_ENCODED_QUEUE_BYTES {
            return Err(QueueCodecError::TooLarge);
        }
        let Json::Object(members) = parse_document(text)? else {
            return Err(QueueCodecError::Malformed);
        };

        let version = match field(&members, "schemaVersion")? {
            Json::Number(version) => *version,
            _ => return Err(QueueCodecError::Malformed),
        };
        if version != QUEUE_SCHEMA_VERSION {
            return Err(QueueCodecError::UnknownVersion(version));
        }
        check_fields(&members)?;

        let repeat = match field(&members, "repeat")? {
            Json::Str(name) => repeat_from_name(name).ok_or(QueueCodecError::Malformed)?,
            _ => return Err(QueueCodecError::Malformed),
        };
        let shuffle = match field(&members, "shuffle")? {
            Json::Bool(shuffle) => *shuffle,
            _ => return Err(QueueCodecError::Malformed),
        };
        let ids = id_array(field(&members, "items")?)?;
        if ids.len() > MAX_QUEUE_LEN {
            return Err(QueueCodecError::TooManyItems);
        }
        for id in &ids {
            check_item_id(id)?;
        }
        let len = ids.len();

        let order = index_array(field(&members, "order")?)?;
        let order = if shuffle {
            if !is_permutation(&order, len) {
                return Err(QueueCodecError::Malformed);
            }
            Some(order)
        } else {
            if !order.is_empty() {
                return Err(QueueCodecError::Malformed);
            }
            None
        };

        let play = match field(&members, "cursor")? {
            Json::Null => None,
            Json::Number(play) => {
                Some(usize::try_from(*play).map_err(|_| QueueCodecError::Malformed)?)
            }
            _ => return Err(QueueCodecError::Malformed),
        };
        let detached = match field(&members, "detached")? {
            Json::Null => None,
            Json::Str(id) => {
                check_item_id(id)?;
                Some(id.clone())
            }
            _ => return Err(QueueCodecError::Malformed),
        };
        let cursor = match (play, detached) {
            (None, None) => None,
            (None, Some(_)) => return Err(QueueCodecError::Malformed),
            (Some(play), detached) => {
                // A live cursor names an item. A detached cursor may name the end of the play
                // order, which is where its successor would start.
                let limit = if detached.is_some() {
                    len
                } else {
                    len.checked_sub(1).ok_or(QueueCodecError::Malformed)?
                };
                if play > limit {
                    return Err(QueueCodecError::Malformed);
                }
                Some(Cursor {
                    play,
                    removed: detached.map(QueueItem::new),
                })
            }
        };

        Ok(Self {
            items: ids.into_iter().map(QueueItem::new).collect(),
            order,
            repeat,
            cursor,
            retried: false,
            rng: SplitMix64(DEFAULT_SEED),
        })
    }
}

fn push_id(out: &mut String, id: &str) {
    out.push('"');
    out.push_str(id);
    out.push('"');
}

fn is_permutation(order: &[usize], len: usize) -> bool {
    if order.len() != len {
        return false;
    }
    let mut seen = vec![false; len];
    for &index in order {
        match seen.get_mut(index) {
            Some(slot) if !*slot => *slot = true,
            _ => return false,
        }
    }
    true
}

/// A field is looked up by name. Missing fields are refused by the caller.
fn field<'a>(members: &'a [(String, Json)], key: &str) -> Result<&'a Json, QueueCodecError> {
    members
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value)
        .ok_or(QueueCodecError::Malformed)
}

/// Every field must be known, appear once, and be present. Unknown fields come from a different
/// writer, so they are refused.
fn check_fields(members: &[(String, Json)]) -> Result<(), QueueCodecError> {
    let mut seen: Vec<&str> = Vec::with_capacity(QUEUE_FIELDS.len());
    for (name, _) in members {
        if !QUEUE_FIELDS.contains(&name.as_str()) || seen.contains(&name.as_str()) {
            return Err(QueueCodecError::Malformed);
        }
        seen.push(name);
    }
    if seen.len() != QUEUE_FIELDS.len() {
        return Err(QueueCodecError::Malformed);
    }
    Ok(())
}

fn id_array(value: &Json) -> Result<Vec<String>, QueueCodecError> {
    let Json::Array(values) = value else {
        return Err(QueueCodecError::Malformed);
    };
    values
        .iter()
        .map(|item| match item {
            Json::Str(id) => Ok(id.clone()),
            _ => Err(QueueCodecError::Malformed),
        })
        .collect()
}

fn index_array(value: &Json) -> Result<Vec<usize>, QueueCodecError> {
    let Json::Array(values) = value else {
        return Err(QueueCodecError::Malformed);
    };
    values
        .iter()
        .map(|item| match item {
            Json::Number(index) => usize::try_from(*index).map_err(|_| QueueCodecError::Malformed),
            _ => Err(QueueCodecError::Malformed),
        })
        .collect()
}

/// The JSON subset the queue format uses. No floats, no escapes in strings, and no negative
/// numbers.
#[derive(Debug, PartialEq, Eq)]
enum Json {
    Null,
    Bool(bool),
    Number(u64),
    Str(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

fn parse_document(text: &str) -> Result<Json, QueueCodecError> {
    let mut reader = JsonReader {
        bytes: text.as_bytes(),
        pos: 0,
    };
    let value = reader.value(0)?;
    reader.skip_ws();
    if reader.pos != reader.bytes.len() {
        return Err(QueueCodecError::Malformed);
    }
    Ok(value)
}

struct JsonReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl JsonReader<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn eat(&mut self, byte: u8) -> Result<(), QueueCodecError> {
        if self.peek() == Some(byte) {
            self.pos += 1;
            Ok(())
        } else {
            Err(QueueCodecError::Malformed)
        }
    }

    fn literal(&mut self, word: &[u8], value: Json) -> Result<Json, QueueCodecError> {
        let matched = self
            .bytes
            .get(self.pos..)
            .is_some_and(|rest| rest.starts_with(word));
        if !matched {
            return Err(QueueCodecError::Malformed);
        }
        self.pos += word.len();
        Ok(value)
    }

    fn value(&mut self, depth: usize) -> Result<Json, QueueCodecError> {
        if depth > MAX_JSON_DEPTH {
            return Err(QueueCodecError::Malformed);
        }
        self.skip_ws();
        match self.peek() {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => self.string().map(Json::Str),
            Some(b'0'..=b'9') => self.number().map(Json::Number),
            Some(b't') => self.literal(b"true", Json::Bool(true)),
            Some(b'f') => self.literal(b"false", Json::Bool(false)),
            Some(b'n') => self.literal(b"null", Json::Null),
            _ => Err(QueueCodecError::Malformed),
        }
    }

    fn string(&mut self) -> Result<String, QueueCodecError> {
        self.eat(b'"')?;
        let start = self.pos;
        loop {
            match self.peek() {
                None => return Err(QueueCodecError::Malformed),
                Some(b'"') => break,
                // Escapes are not part of the format. Item IDs never need one.
                Some(b'\\') => return Err(QueueCodecError::Malformed),
                Some(byte) if byte < 0x20 => return Err(QueueCodecError::Malformed),
                Some(_) => self.pos += 1,
            }
        }
        let raw = self
            .bytes
            .get(start..self.pos)
            .ok_or(QueueCodecError::Malformed)?;
        let text = std::str::from_utf8(raw).map_err(|_| QueueCodecError::Malformed)?;
        self.pos += 1;
        Ok(text.to_owned())
    }

    fn number(&mut self) -> Result<u64, QueueCodecError> {
        let start = self.pos;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        let raw = self
            .bytes
            .get(start..self.pos)
            .ok_or(QueueCodecError::Malformed)?;
        // No leading zeros, so each number has one spelling.
        if raw.len() > 1 && raw.first() == Some(&b'0') {
            return Err(QueueCodecError::Malformed);
        }
        let text = std::str::from_utf8(raw).map_err(|_| QueueCodecError::Malformed)?;
        text.parse::<u64>().map_err(|_| QueueCodecError::Malformed)
    }

    fn array(&mut self, depth: usize) -> Result<Json, QueueCodecError> {
        self.eat(b'[')?;
        let mut values = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Json::Array(values));
        }
        loop {
            values.push(self.value(depth + 1)?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Json::Array(values));
                }
                _ => return Err(QueueCodecError::Malformed),
            }
        }
    }

    fn object(&mut self, depth: usize) -> Result<Json, QueueCodecError> {
        self.eat(b'{')?;
        let mut members = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Json::Object(members));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            self.eat(b':')?;
            let value = self.value(depth + 1)?;
            members.push((key, value));
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Json::Object(members));
                }
                _ => return Err(QueueCodecError::Malformed),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use jellyfin_api::session_reporting::{PlaybackOrder, QueueItem, RepeatMode};

    use super::{
        MAX_ENCODED_QUEUE_BYTES, MAX_QUEUE_LEN, Queue, QueueCodecError, QueueError,
        RESTART_THRESHOLD, Step,
    };

    fn item(n: usize) -> QueueItem {
        QueueItem::new(format!("{n:032x}"))
    }

    fn queue_of(n: usize) -> Queue {
        let mut queue = Queue::with_seed(7);
        for n in 0..n {
            queue.add(item(n)).expect("add");
        }
        queue
    }

    fn ids(queue: &Queue) -> Vec<String> {
        queue
            .play_order()
            .into_iter()
            .map(|index| queue.items()[index].id.clone())
            .collect()
    }

    fn sorted(values: &[usize]) -> Vec<usize> {
        let mut values = values.to_vec();
        values.sort_unstable();
        values
    }

    #[test]
    fn empty_queue_steps_do_nothing() {
        let mut queue = Queue::new();
        assert_eq!(queue.next_track(), Step::Empty);
        assert_eq!(queue.previous_track(Duration::ZERO), Step::Empty);
        assert_eq!(queue.track_ended(), Step::End);
        assert_eq!(queue.track_failed(), Step::End);
        assert!(queue.current().is_none());
        assert!(queue.play_order().is_empty());
    }

    #[test]
    fn first_step_starts_at_first_item() {
        let mut queue = queue_of(3);
        assert_eq!(queue.next_track(), Step::Changed(0));
        assert_eq!(queue.current().map(|i| i.id.clone()), Some(item(0).id));
    }

    #[test]
    fn repeat_off_stops_at_the_end() {
        let mut queue = queue_of(3);
        assert_eq!(queue.next_track(), Step::Changed(0));
        assert_eq!(queue.next_track(), Step::Changed(1));
        assert_eq!(queue.next_track(), Step::Changed(2));
        assert_eq!(queue.next_track(), Step::End);
        assert_eq!(queue.current_index(), Some(2));
        assert_eq!(queue.next_track(), Step::End);
    }

    #[test]
    fn repeat_all_wraps_last_to_first() {
        let mut queue = queue_of(3);
        queue.set_repeat(RepeatMode::All);
        assert_eq!(queue.next_track(), Step::Changed(0));
        assert_eq!(queue.next_track(), Step::Changed(1));
        assert_eq!(queue.next_track(), Step::Changed(2));
        assert_eq!(queue.next_track(), Step::Changed(0));
    }

    #[test]
    fn repeat_all_single_item_repeats_itself() {
        let mut queue = queue_of(1);
        queue.set_repeat(RepeatMode::All);
        assert_eq!(queue.next_track(), Step::Changed(0));
        assert_eq!(queue.next_track(), Step::Changed(0));
    }

    #[test]
    fn repeat_one_track_end_replays_current() {
        let mut queue = queue_of(3);
        queue.set_repeat(RepeatMode::One);
        assert_eq!(queue.next_track(), Step::Changed(0));
        assert_eq!(queue.track_ended(), Step::Restart);
        assert_eq!(queue.current_index(), Some(0));
        assert_eq!(queue.track_ended(), Step::Restart);
        assert_eq!(queue.current_index(), Some(0));
    }

    #[test]
    fn repeat_one_user_next_moves_on_and_stops_at_end() {
        let mut queue = queue_of(2);
        queue.set_repeat(RepeatMode::One);
        assert_eq!(queue.next_track(), Step::Changed(0));
        assert_eq!(queue.next_track(), Step::Changed(1));
        assert_eq!(queue.next_track(), Step::End);
    }

    #[test]
    fn track_end_advances_in_off_and_all() {
        let mut queue = queue_of(2);
        assert_eq!(queue.next_track(), Step::Changed(0));
        assert_eq!(queue.track_ended(), Step::Changed(1));
        assert_eq!(queue.track_ended(), Step::End);

        let mut queue = queue_of(2);
        queue.set_repeat(RepeatMode::All);
        assert_eq!(queue.next_track(), Step::Changed(0));
        assert_eq!(queue.track_ended(), Step::Changed(1));
        assert_eq!(queue.track_ended(), Step::Changed(0));
    }

    #[test]
    fn repeat_one_failure_retries_once_then_skips() {
        let mut queue = queue_of(3);
        queue.set_repeat(RepeatMode::One);
        assert_eq!(queue.next_track(), Step::Changed(0));
        assert_eq!(queue.track_failed(), Step::Restart);
        assert_eq!(queue.track_failed(), Step::Changed(1));
        // The retry allowance belongs to each item, so the next item gets one too.
        assert_eq!(queue.track_failed(), Step::Restart);
        assert_eq!(queue.track_failed(), Step::Changed(2));
    }

    #[test]
    fn failure_with_repeat_off_or_all_skips_without_retry() {
        let mut queue = queue_of(3);
        assert_eq!(queue.next_track(), Step::Changed(0));
        assert_eq!(queue.track_failed(), Step::Changed(1));

        let mut queue = queue_of(3);
        queue.set_repeat(RepeatMode::All);
        assert_eq!(queue.next_track(), Step::Changed(0));
        assert_eq!(queue.track_failed(), Step::Changed(1));
    }

    #[test]
    fn previous_restarts_only_after_the_threshold() {
        let mut queue = queue_of(3);
        queue.next_track();
        queue.next_track();
        let over = RESTART_THRESHOLD + Duration::from_millis(1);
        assert_eq!(queue.previous_track(over), Step::Restart);
        assert_eq!(queue.current_index(), Some(1));
        // Exactly the threshold is not more than the threshold: go to the prior track.
        assert_eq!(queue.previous_track(RESTART_THRESHOLD), Step::Changed(0));
        assert_eq!(queue.previous_track(Duration::ZERO), Step::Restart);
        assert_eq!(queue.current_index(), Some(0));
    }

    #[test]
    fn previous_at_start_wraps_only_under_repeat_all() {
        let mut queue = queue_of(3);
        queue.set_repeat(RepeatMode::All);
        queue.next_track();
        assert_eq!(queue.previous_track(Duration::ZERO), Step::Changed(2));

        let mut queue = queue_of(3);
        queue.next_track();
        assert_eq!(queue.previous_track(Duration::ZERO), Step::Restart);
    }

    #[test]
    fn previous_with_no_current_starts_first_item() {
        let mut queue = queue_of(3);
        assert_eq!(queue.previous_track(Duration::ZERO), Step::Changed(0));
    }

    #[test]
    fn add_appends_and_keeps_current() {
        let mut queue = queue_of(2);
        queue.next_track();
        queue.add(item(9)).expect("add");
        assert_eq!(queue.current_index(), Some(0));
        assert_eq!(queue.play_order(), vec![0, 1, 2]);
        assert_eq!(queue.next_track(), Step::Changed(1));
    }

    #[test]
    fn play_next_inserts_after_current_and_keeps_current() {
        let mut queue = queue_of(3);
        queue.next_track();
        queue.play_next(item(9)).expect("play next");
        assert_eq!(queue.items()[1].id, item(9).id);
        assert_eq!(queue.current_index(), Some(0));
        assert_eq!(queue.next_track(), Step::Changed(1));
        assert_eq!(queue.next_track(), Step::Changed(2));
    }

    #[test]
    fn play_next_without_current_goes_to_front() {
        let mut queue = Queue::new();
        queue.play_next(item(9)).expect("play next");
        assert_eq!(queue.items()[0].id, item(9).id);
        assert_eq!(queue.next_track(), Step::Changed(0));
    }

    #[test]
    fn album_plays_from_its_first_track() {
        let mut queue = queue_of(2);
        let album = vec![item(10), item(11), item(12)];
        queue.add_album_and_play(album).expect("album");
        assert_eq!(queue.len(), 5);
        assert_eq!(queue.current_index(), Some(2));
        assert_eq!(queue.current().map(|i| i.id.clone()), Some(item(10).id));
        assert_eq!(queue.next_track(), Step::Changed(3));
    }

    #[test]
    fn shuffled_album_keeps_its_tracks_in_album_order() {
        let mut queue = queue_of(4);
        queue.set_shuffle(true);
        queue
            .add_album_and_play(vec![item(10), item(11), item(12)])
            .expect("album");

        assert_eq!(queue.current_index(), Some(4));
        assert_eq!(queue.next_track(), Step::Changed(5));
        assert_eq!(queue.next_track(), Step::Changed(6));
    }

    #[test]
    fn empty_album_is_rejected() {
        let mut queue = queue_of(1);
        assert_eq!(
            queue.add_album_and_play(Vec::new()),
            Err(QueueError::NoItems)
        );
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn remove_before_current_shifts_current_index() {
        let mut queue = queue_of(4);
        queue.next_track();
        queue.next_track();
        queue.remove(0).expect("remove");
        assert_eq!(queue.current_index(), Some(0));
        assert_eq!(queue.current().map(|i| i.id.clone()), Some(item(1).id));
    }

    #[test]
    fn remove_after_current_keeps_current_index() {
        let mut queue = queue_of(4);
        queue.next_track();
        queue.remove(3).expect("remove");
        assert_eq!(queue.len(), 3);
        assert_eq!(queue.current_index(), Some(0));
    }

    #[test]
    fn reconciliation_preserves_shuffled_order_and_retained_current() {
        let mut queue = queue_of(8);
        queue.set_shuffle(true);
        let old_order = ids(&queue);
        let current_play = 3;
        let current_id = old_order[current_play].clone();
        let selected = queue.play_order()[current_play];
        queue.select(selected).expect("select");
        queue.set_repeat(RepeatMode::All);

        let removed = queue.retain_items(|entry| entry.id != item(1).id && entry.id != item(5).id);

        let expected_order: Vec<_> = old_order
            .into_iter()
            .filter(|id| id != &item(1).id && id != &item(5).id)
            .collect();
        assert_eq!(removed, 2);
        assert_eq!(ids(&queue), expected_order);
        assert_eq!(
            queue.current().map(|entry| entry.id.as_str()),
            Some(current_id.as_str())
        );
        assert_eq!(queue.repeat(), RepeatMode::All);
        assert!(queue.is_shuffled());
    }

    #[test]
    fn reconciliation_clamps_missing_current_and_clears_detached_item() {
        let mut queue = queue_of(5);
        queue.next_track();
        queue.next_track();
        queue.next_track();
        assert_eq!(
            queue.current().map(|entry| entry.id.as_str()),
            Some(item(2).id.as_str())
        );

        let removed = queue.retain_items(|entry| entry.id != item(2).id && entry.id != item(4).id);

        assert_eq!(removed, 2);
        assert_eq!(queue.current_index(), Some(2));
        assert_eq!(
            queue.current().map(|entry| entry.id.as_str()),
            Some(item(3).id.as_str())
        );
        assert!(queue.removed_current().is_none());

        queue.remove(2).expect("remove current");
        assert!(queue.removed_current().is_some());
        assert_eq!(queue.retain_items(|_| true), 0);
        assert!(queue.removed_current().is_none());
        assert_eq!(queue.current_index(), Some(1));
    }

    #[test]
    fn reconciliation_to_empty_clears_cursor_and_shuffle_order() {
        let mut queue = queue_of(4);
        queue.set_shuffle(true);
        queue.next_track();
        queue.set_repeat(RepeatMode::All);

        assert_eq!(queue.retain_items(|_| false), 4);

        assert!(queue.is_empty());
        assert!(queue.play_order().is_empty());
        assert!(queue.current_index().is_none());
        assert!(queue.removed_current().is_none());
        assert_eq!(queue.next_track(), Step::Empty);
        assert_eq!(queue.repeat(), RepeatMode::All);
        assert!(queue.is_shuffled());
    }

    #[test]
    fn reconciliation_preserves_duplicate_accepted_entries() {
        let duplicate_id = item(7).id;
        let mut queue = Queue::new();
        queue
            .add(QueueItem::new(duplicate_id.clone()))
            .expect("add");
        queue
            .add(QueueItem::new(duplicate_id.clone()))
            .expect("add");
        queue.add(item(8)).expect("add");

        assert_eq!(queue.retain_items(|entry| entry.id == duplicate_id), 1);

        assert_eq!(queue.len(), 2);
        assert_eq!(queue.items()[0].id, duplicate_id);
        assert_eq!(queue.items()[1].id, item(7).id);
    }

    #[test]
    fn reconciliation_with_no_removals_leaves_queue_state_unchanged() {
        let mut queue = queue_of(6);
        queue.set_shuffle(true);
        queue.select(queue.play_order()[2]).expect("select");
        queue.set_repeat(RepeatMode::One);
        let original_order = queue.play_order();
        let original_ids = ids(&queue);
        let original_current = queue.current().map(|entry| entry.id.clone());

        assert_eq!(queue.retain_items(|_| true), 0);

        assert_eq!(queue.play_order(), original_order);
        assert_eq!(ids(&queue), original_ids);
        assert_eq!(
            queue.current().map(|entry| entry.id.clone()),
            original_current
        );
        assert_eq!(queue.repeat(), RepeatMode::One);
        assert!(queue.is_shuffled());
    }

    #[test]
    fn remove_out_of_range_is_rejected() {
        let mut queue = queue_of(2);
        assert_eq!(queue.remove(2).err(), Some(QueueError::OutOfRange));
        assert_eq!(queue.len(), 2);
    }

    #[test]
    fn removing_current_keeps_it_playing_until_it_ends() {
        let mut queue = queue_of(4);
        queue.next_track();
        queue.next_track();
        let removed = queue.remove(1).expect("remove");
        assert_eq!(removed.id, item(1).id);
        assert!(queue.current().is_none());
        assert_eq!(
            queue.removed_current().map(|i| i.id.clone()),
            Some(item(1).id)
        );
        assert_eq!(queue.track_ended(), Step::Changed(1));
        assert_eq!(queue.current().map(|i| i.id.clone()), Some(item(2).id));
        assert!(queue.removed_current().is_none());
    }

    #[test]
    fn removed_current_does_not_repeat_under_repeat_one() {
        let mut queue = queue_of(3);
        queue.set_repeat(RepeatMode::One);
        queue.next_track();
        queue.remove(0).expect("remove");
        assert_eq!(queue.track_ended(), Step::Changed(0));
        assert_eq!(queue.current().map(|i| i.id.clone()), Some(item(1).id));
    }

    #[test]
    fn removing_last_item_while_playing_ends_queue() {
        let mut queue = queue_of(2);
        queue.next_track();
        queue.next_track();
        queue.remove(1).expect("remove");
        assert_eq!(queue.track_ended(), Step::End);
        assert_eq!(queue.current_index(), Some(0));
    }

    #[test]
    fn removing_only_item_empties_the_queue() {
        let mut queue = queue_of(1);
        queue.next_track();
        queue.remove(0).expect("remove");
        assert!(queue.is_empty());
        assert_eq!(queue.next_track(), Step::Empty);
        assert!(queue.removed_current().is_none());
    }

    #[test]
    fn shuffle_keeps_current_item_and_does_not_restart_it() {
        let mut queue = queue_of(10);
        queue.next_track();
        queue.next_track();
        queue.next_track();
        queue.set_shuffle(true);
        assert!(queue.is_shuffled());
        assert_eq!(queue.playback_order(), PlaybackOrder::Shuffle);
        assert_eq!(queue.current_index(), Some(2));
        assert_eq!(queue.play_order()[0], 2);
    }

    #[test]
    fn shuffle_order_is_a_permutation() {
        let mut queue = queue_of(10);
        queue.set_shuffle(true);
        assert_eq!(sorted(&queue.play_order()), (0..10).collect::<Vec<_>>());
    }

    #[test]
    fn shuffle_is_deterministic_for_a_seed() {
        let mut first = Queue::with_seed(42);
        let mut second = Queue::with_seed(42);
        for n in 0..20 {
            first.add(item(n)).expect("add");
            second.add(item(n)).expect("add");
        }
        first.set_shuffle(true);
        second.set_shuffle(true);
        assert_eq!(first.play_order(), second.play_order());
    }

    #[test]
    fn different_seeds_give_different_orders() {
        let mut first = Queue::with_seed(1);
        let mut second = Queue::with_seed(2);
        for n in 0..10 {
            first.add(item(n)).expect("add");
            second.add(item(n)).expect("add");
        }
        first.set_shuffle(true);
        second.set_shuffle(true);
        assert_ne!(first.play_order(), second.play_order());
    }

    #[test]
    fn turning_shuffle_off_returns_to_list_order_at_current_item() {
        let mut queue = queue_of(10);
        queue.set_shuffle(true);
        queue.next_track();
        queue.next_track();
        queue.next_track();
        let current = queue.current_index().expect("current");
        queue.set_shuffle(false);
        assert!(!queue.is_shuffled());
        assert_eq!(queue.playback_order(), PlaybackOrder::Default);
        assert_eq!(queue.play_order(), (0..10).collect::<Vec<_>>());
        assert_eq!(queue.current_index(), Some(current));
    }

    #[test]
    fn shuffled_walk_visits_every_item_once_before_end() {
        let mut queue = queue_of(10);
        queue.set_shuffle(true);
        let mut visited = Vec::new();
        loop {
            match queue.next_track() {
                Step::Changed(index) => visited.push(index),
                Step::End => break,
                other => panic!("unexpected step {other:?}"),
            }
        }
        assert_eq!(sorted(&visited), (0..10).collect::<Vec<_>>());
    }

    #[test]
    fn shuffle_with_one_item_is_a_no_op() {
        let mut queue = queue_of(1);
        queue.set_shuffle(true);
        assert!(!queue.is_shuffled());
        assert_eq!(queue.play_order(), vec![0]);
    }

    #[test]
    fn add_while_shuffled_regenerates_the_tail_and_keeps_current_first() {
        let mut queue = queue_of(3);
        queue.set_shuffle(true);
        queue.next_track();
        let current = queue.current_index().expect("current");
        let previous_order = queue.play_order();
        queue.add(item(9)).expect("add");
        let order = queue.play_order();
        assert_eq!(order.len(), 4);
        assert_eq!(order[0], current);
        assert_ne!(
            order,
            previous_order.into_iter().chain([3]).collect::<Vec<_>>()
        );
        assert_eq!(sorted(&order), vec![0, 1, 2, 3]);
    }

    #[test]
    fn play_next_while_shuffled_plays_next() {
        let mut queue = queue_of(5);
        queue.set_shuffle(true);
        queue.next_track();
        queue.play_next(item(9)).expect("play next");
        assert_eq!(queue.next_track(), Step::Changed(5));
    }

    #[test]
    fn remove_while_shuffled_regenerates_permutation_and_keeps_current() {
        let mut queue = queue_of(6);
        queue.set_shuffle(true);
        queue.next_track();
        let current = queue.current_index().expect("current");
        let removed_id = item(2).id;
        queue.remove(2).expect("remove");
        let after = ids(&queue);
        assert_eq!(after[0], item(current).id);
        assert!(!after.contains(&removed_id));
        assert_eq!(after.len(), 5);
    }

    #[test]
    fn move_item_reorders_list_and_keeps_current_track() {
        let mut queue = queue_of(4);
        queue.next_track();
        queue.next_track();
        queue.move_item(1, 3).expect("move");
        assert_eq!(queue.items()[3].id, item(1).id);
        assert_eq!(queue.current_index(), Some(3));
        assert_eq!(queue.current().map(|i| i.id.clone()), Some(item(1).id));
    }

    #[test]
    fn move_item_while_shuffled_regenerates_tail_and_keeps_current() {
        let mut queue = queue_of(5);
        queue.set_shuffle(true);
        queue.next_track();
        let current = queue.current_index().expect("current");
        let before = ids(&queue);
        queue.move_item(0, 4).expect("move");
        let after = ids(&queue);
        assert_eq!(after[0], item(current).id);
        assert_eq!(sorted(&queue.play_order()), vec![0, 1, 2, 3, 4]);
        assert_ne!(after, before);
    }

    #[test]
    fn move_item_rejects_out_of_range() {
        let mut queue = queue_of(2);
        assert_eq!(queue.move_item(0, 2), Err(QueueError::OutOfRange));
        assert_eq!(queue.items()[0].id, item(0).id);
    }

    #[test]
    fn queue_rejects_items_past_the_bound() {
        let mut queue = queue_of(MAX_QUEUE_LEN);
        assert_eq!(queue.add(item(MAX_QUEUE_LEN)), Err(QueueError::Full));
        assert_eq!(queue.len(), MAX_QUEUE_LEN);
        assert_eq!(
            queue.add_album_and_play(vec![item(1), item(2)]),
            Err(QueueError::Full)
        );
        assert_eq!(queue.len(), MAX_QUEUE_LEN);
    }

    #[test]
    fn rules_hold_for_a_two_hundred_item_queue() {
        let mut queue = queue_of(200);
        for expected in 0..200 {
            assert_eq!(queue.next_track(), Step::Changed(expected));
        }
        assert_eq!(queue.next_track(), Step::End);

        queue.set_repeat(RepeatMode::All);
        assert_eq!(queue.next_track(), Step::Changed(0));

        queue.set_shuffle(true);
        assert_eq!(queue.current_index(), Some(0));
        assert_eq!(queue.play_order()[0], 0);
        let mut visited = Vec::new();
        for _ in 0..200 {
            match queue.next_track() {
                Step::Changed(index) => visited.push(index),
                other => panic!("unexpected step {other:?}"),
            }
        }
        assert_eq!(sorted(&visited), (0..200).collect::<Vec<_>>());

        queue.set_shuffle(false);
        assert_eq!(queue.play_order(), (0..200).collect::<Vec<_>>());
    }

    #[test]
    fn default_repeat_is_off_and_maps_to_default_order() {
        let queue = Queue::new();
        assert_eq!(queue.repeat(), RepeatMode::None);
        assert_eq!(queue.playback_order(), PlaybackOrder::Default);
    }

    fn list_ids(queue: &Queue) -> Vec<String> {
        queue.items().iter().map(|item| item.id.clone()).collect()
    }

    fn assert_same_state(original: &Queue, restored: &Queue) {
        assert_eq!(list_ids(original), list_ids(restored));
        assert_eq!(original.play_order(), restored.play_order());
        assert_eq!(original.current_index(), restored.current_index());
        assert_eq!(
            original.removed_current().map(|item| item.id.clone()),
            restored.removed_current().map(|item| item.id.clone())
        );
        assert_eq!(original.repeat(), restored.repeat());
        assert_eq!(original.is_shuffled(), restored.is_shuffled());
    }

    /// Build a document by hand, so that tests can break one field at a time.
    fn document(
        version: &str,
        repeat: &str,
        shuffle: bool,
        ids: &[String],
        order: &str,
        cursor: &str,
        detached: &str,
    ) -> String {
        let items = ids
            .iter()
            .map(|id| format!("\"{id}\""))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "{{\"schemaVersion\":{version},\"repeat\":\"{repeat}\",\"shuffle\":{shuffle},\"items\":[{items}],\"order\":[{order}],\"cursor\":{cursor},\"detached\":{detached}}}"
        )
    }

    fn two_ids() -> Vec<String> {
        vec![format!("{:032x}", 0), format!("{:032x}", 1)]
    }

    #[test]
    fn two_hundred_item_queue_survives_encode_and_decode_in_order_and_cursor() {
        let mut queue = queue_of(200);
        queue.set_repeat(RepeatMode::All);
        queue.set_shuffle(true);
        for _ in 0..57 {
            queue.next_track();
        }
        let text = queue.encode().expect("encode");
        let mut restored = Queue::decode(&text).expect("decode");

        assert_eq!(restored.len(), 200);
        assert_same_state(&queue, &restored);
        for _ in 0..400 {
            assert_eq!(restored.next_track(), queue.next_track());
        }
        assert_same_state(&queue, &restored);
    }

    #[test]
    fn list_order_queue_at_its_last_item_round_trips() {
        let mut queue = queue_of(3);
        for _ in 0..3 {
            queue.next_track();
        }
        assert_eq!(queue.next_track(), Step::End);
        let restored = Queue::decode(&queue.encode().expect("encode")).expect("decode");
        assert_same_state(&queue, &restored);
        assert!(!restored.is_shuffled());
    }

    #[test]
    fn detached_current_item_round_trips_and_keeps_its_successor() {
        let mut queue = queue_of(5);
        queue.set_repeat(RepeatMode::One);
        queue.next_track();
        queue.next_track();
        queue.remove(1).expect("remove the playing item");
        assert!(queue.removed_current().is_some());

        let mut restored = Queue::decode(&queue.encode().expect("encode")).expect("decode");
        assert_same_state(&queue, &restored);
        assert_eq!(restored.next_track(), queue.next_track());
    }

    #[test]
    fn empty_queue_round_trips() {
        let queue = Queue::new();
        let restored = Queue::decode(&queue.encode().expect("encode")).expect("decode");
        assert!(restored.is_empty());
        assert_eq!(restored.repeat(), RepeatMode::None);
        assert!(!restored.is_shuffled());
    }

    #[test]
    fn unknown_version_is_refused() {
        let text = document("2", "off", false, &two_ids(), "", "null", "null");
        assert!(matches!(
            Queue::decode(&text).err(),
            Some(QueueCodecError::UnknownVersion(2))
        ));
        let missing = "{\"repeat\":\"off\"}";
        assert_eq!(
            Queue::decode(missing).err(),
            Some(QueueCodecError::Malformed)
        );
    }

    #[test]
    fn unknown_repeated_or_missing_fields_are_refused() {
        let extra = "{\"schemaVersion\":1,\"repeat\":\"off\",\"shuffle\":false,\"items\":[],\"order\":[],\"cursor\":null,\"detached\":null,\"extra\":1}";
        assert_eq!(Queue::decode(extra).err(), Some(QueueCodecError::Malformed));

        let repeated = "{\"schemaVersion\":1,\"schemaVersion\":1,\"repeat\":\"off\",\"shuffle\":false,\"items\":[],\"order\":[],\"cursor\":null,\"detached\":null}";
        assert_eq!(
            Queue::decode(repeated).err(),
            Some(QueueCodecError::Malformed)
        );

        let missing = "{\"schemaVersion\":1,\"repeat\":\"off\",\"shuffle\":false,\"items\":[],\"order\":[],\"cursor\":null}";
        assert_eq!(
            Queue::decode(missing).err(),
            Some(QueueCodecError::Malformed)
        );
    }

    #[test]
    fn malformed_text_is_refused() {
        let good = Queue::new().encode().expect("encode");
        for broken in [
            "".to_owned(),
            "[]".to_owned(),
            format!("{good} trailing"),
            good.replace('}', ",}"),
            good.replace("\"repeat\":\"off\"", "\"repeat\":\"sometimes\""),
            good.replace("\"shuffle\":false", "\"shuffle\":0"),
            good.replace("\"cursor\":null", "\"cursor\":-1"),
            good.replace("\"cursor\":null", "\"cursor\":01"),
            good.replace("\"order\":[]", "\"order\":[1.5]"),
        ] {
            assert_eq!(
                Queue::decode(&broken).err(),
                Some(QueueCodecError::Malformed),
                "{broken}"
            );
        }
    }

    #[test]
    fn inconsistent_fields_are_refused() {
        let ids = two_ids();
        // Shuffle on, but the order repeats an index.
        let repeated_order = document("1", "off", true, &ids, "0,0", "null", "null");
        assert_eq!(
            Queue::decode(&repeated_order).err(),
            Some(QueueCodecError::Malformed)
        );
        // Shuffle off, but an order is present.
        let order_without_shuffle = document("1", "off", false, &ids, "1,0", "null", "null");
        assert_eq!(
            Queue::decode(&order_without_shuffle).err(),
            Some(QueueCodecError::Malformed)
        );
        // A live cursor past the last item.
        let cursor_past_end = document("1", "off", false, &ids, "", "2", "null");
        assert_eq!(
            Queue::decode(&cursor_past_end).err(),
            Some(QueueCodecError::Malformed)
        );
        // A detached item with no cursor.
        let detached_without_cursor = document("1", "off", false, &ids, "", "null", "\"x\"");
        assert_eq!(
            Queue::decode(&detached_without_cursor).err(),
            Some(QueueCodecError::Malformed)
        );
        // A detached cursor may sit at the end of the play order.
        let detached_at_end = document("1", "off", false, &ids, "", "2", "\"x\"");
        assert!(Queue::decode(&detached_at_end).is_ok());
    }

    #[test]
    fn size_and_item_count_are_bounded() {
        let oversized = "x".repeat(MAX_ENCODED_QUEUE_BYTES + 1);
        assert_eq!(
            Queue::decode(&oversized).err(),
            Some(QueueCodecError::TooLarge)
        );

        let ids: Vec<String> = (0..=MAX_QUEUE_LEN).map(|n| format!("{n:032x}")).collect();
        let too_many = document("1", "off", false, &ids, "", "null", "null");
        assert_eq!(
            Queue::decode(&too_many).err(),
            Some(QueueCodecError::TooManyItems)
        );
    }

    #[test]
    fn unsafe_item_ids_are_refused_both_ways() {
        let mut queue = Queue::new();
        queue
            .add(QueueItem::new("https://media.example/Audio/1"))
            .expect("add");
        assert_eq!(queue.encode().err(), Some(QueueCodecError::UnsafeId));

        let mut keyed = Queue::new();
        keyed.add(QueueItem::new("api_key")).expect("add");
        assert_eq!(keyed.encode().err(), Some(QueueCodecError::UnsafeId));

        let slash = document("1", "off", false, &["a/b".to_owned()], "", "null", "null");
        assert_eq!(Queue::decode(&slash).err(), Some(QueueCodecError::UnsafeId));
        let escaped = "{\"schemaVersion\":1,\"repeat\":\"off\",\"shuffle\":false,\"items\":[\"a\\u0041\"],\"order\":[],\"cursor\":null,\"detached\":null}";
        assert_eq!(
            Queue::decode(escaped).err(),
            Some(QueueCodecError::Malformed)
        );
    }

    #[test]
    fn encoded_queue_contains_no_url_or_credential_text() {
        let mut queue = queue_of(200);
        queue.set_shuffle(true);
        queue.set_repeat(RepeatMode::All);
        let text = queue.encode().expect("encode");
        for forbidden in ["http", "api_key", "token", "://", "password"] {
            assert!(
                !text.contains(forbidden),
                "encoded queue contains {forbidden}"
            );
        }
        assert!(text.len() < 16 * 1024, "200 IDs stay small: {}", text.len());
    }
}

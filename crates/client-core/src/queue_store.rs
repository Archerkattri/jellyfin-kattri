//! Save and load the music queue as a file, using the versioned codec in [`crate::queue`].
//!
//! Save path: encode -> write `.<name>.<unique>.tmp` in the same directory -> fsync -> rename
//! over the target. A failure before the rename leaves the previous file untouched.
//!
//! Load path: read at most [`MAX_ENCODED_QUEUE_BYTES`] plus one byte -> decode. A missing
//! file is reported as [`LoadOutcome::Missing`]. A file that cannot be decoded, including one
//! with an unknown schema version, is renamed to `<name>.corrupt` (or `<name>.corrupt.<n>` when
//! that name is taken) and reported as [`LoadOutcome::MovedAside`]. Nothing is deleted.
//!
//! The file holds only what the codec writes: item IDs (restricted to ASCII letters, digits,
//! `-` and `_`, and checked against credential-like names), the shuffle order, the repeat
//! mode, and the cursor. It holds no tokens, passwords, or URLs.

use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::queue::{MAX_ENCODED_QUEUE_BYTES, Queue, QueueCodecError};

/// Suffix added to a file that could not be loaded.
const CORRUPT_SUFFIX: &str = ".corrupt";

/// How many numbered `.corrupt.<n>` names are tried before giving up.
const MAX_ASIDE_ATTEMPTS: u32 = 1_000;

/// Store schema version that adds the playback offset to the queue codec.
const STORED_QUEUE_SCHEMA_VERSION: u64 = 2;

/// Process-wide counter that keeps temporary names distinct within one process.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Result of [`load_queue`].
pub enum LoadOutcome {
    /// No file exists at the path.
    Missing,
    /// The file decoded into a queue.
    Loaded(Queue),
    /// The file could not be used. It was renamed to `moved_to` and left in place there.
    MovedAside {
        /// Why the codec refused the file. [`QueueCodecError::UnknownVersion`] carries the
        /// version found.
        reason: QueueCodecError,
        /// Where the file now lives.
        moved_to: PathBuf,
    },
}

/// A queue and its last persisted playback offset.
#[derive(Clone)]
pub struct StoredQueue {
    /// Queue order, cursor, shuffle, and repeat state.
    pub queue: Queue,
    /// Last saved playback offset, in milliseconds.
    pub position_ms: u64,
}

/// Result of loading a position-aware queue file.
pub enum StoredLoadOutcome {
    /// No file exists at the path.
    Missing,
    /// The file decoded into a queue snapshot.
    Loaded(StoredQueue),
    /// The file could not be used and was moved aside.
    MovedAside {
        /// Why the codec refused the file.
        reason: QueueCodecError,
        /// Where the file now lives.
        moved_to: PathBuf,
    },
}

impl fmt::Debug for StoredLoadOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing => f.write_str("Missing"),
            Self::Loaded(stored) => f
                .debug_struct("Loaded")
                .field("position_ms", &stored.position_ms)
                .field("queue", &"Queue(..)")
                .finish(),
            Self::MovedAside { reason, moved_to } => f
                .debug_struct("MovedAside")
                .field("reason", reason)
                .field("moved_to", moved_to)
                .finish(),
        }
    }
}

impl fmt::Debug for LoadOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing => f.write_str("Missing"),
            Self::Loaded(_) => f.write_str("Loaded(..)"),
            Self::MovedAside { reason, moved_to } => f
                .debug_struct("MovedAside")
                .field("reason", reason)
                .field("moved_to", moved_to)
                .finish(),
        }
    }
}

/// Failures of [`save_queue`] and [`load_queue`]. A corrupt file is not an error; see
/// [`LoadOutcome::MovedAside`].
#[derive(Debug)]
pub enum QueueStoreError {
    /// Reading, writing, syncing, or renaming a file failed.
    Io(io::Error),
    /// The queue could not be encoded, for example because an item ID is unsafe.
    Encode(QueueCodecError),
}

impl fmt::Display for QueueStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "queue file I/O failed: {err}"),
            Self::Encode(err) => write!(f, "queue could not be encoded: {err:?}"),
        }
    }
}

impl std::error::Error for QueueStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Encode(_) => None,
        }
    }
}

impl From<io::Error> for QueueStoreError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

/// Encode `queue` and write it to `path` atomically.
///
/// The parent directory is created if needed. On success the file holds the new queue. On
/// failure the previous file, if any, is unchanged and no temporary file is left behind.
pub fn save_queue(path: &Path, queue: &Queue) -> Result<(), QueueStoreError> {
    save_queue_with_position(path, queue, 0)
}

/// Encode `queue` and its playback offset and write the result atomically.
pub fn save_queue_with_position(
    path: &Path,
    queue: &Queue,
    position_ms: u64,
) -> Result<(), QueueStoreError> {
    let text = encode_stored_queue(queue, position_ms).map_err(QueueStoreError::Encode)?;
    let Some(file_name) = path.file_name() else {
        return Err(QueueStoreError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "queue path has no file name",
        )));
    };
    if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        fs::create_dir_all(dir)?;
    }
    let temp = path.with_file_name(temp_file_name(file_name));
    write_atomic(path, &text, &temp)?;
    Ok(())
}

/// Read the queue stored at `path`.
///
/// Returns [`LoadOutcome::Missing`] when there is no file, [`LoadOutcome::Loaded`] when the
/// file decodes, and [`LoadOutcome::MovedAside`] when it does not. An I/O failure while
/// reading or moving the file is returned as an error.
pub fn load_queue(path: &Path) -> Result<LoadOutcome, QueueStoreError> {
    Ok(match load_queue_with_position(path)? {
        StoredLoadOutcome::Missing => LoadOutcome::Missing,
        StoredLoadOutcome::Loaded(stored) => LoadOutcome::Loaded(stored.queue),
        StoredLoadOutcome::MovedAside { reason, moved_to } => {
            LoadOutcome::MovedAside { reason, moved_to }
        }
    })
}

/// Read a queue and its last playback offset. Files written before position persistence are
/// accepted with position zero.
pub fn load_queue_with_position(path: &Path) -> Result<StoredLoadOutcome, QueueStoreError> {
    let bytes = match read_capped(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(StoredLoadOutcome::Missing),
        Err(err) => return Err(QueueStoreError::Io(err)),
    };
    let decoded = if bytes.len() > MAX_ENCODED_QUEUE_BYTES {
        Err(QueueCodecError::TooLarge)
    } else {
        match std::str::from_utf8(&bytes) {
            Ok(text) => decode_stored_queue(text),
            Err(_) => Err(QueueCodecError::Malformed),
        }
    };
    match decoded {
        Ok(stored) => Ok(StoredLoadOutcome::Loaded(stored)),
        Err(reason) => {
            let moved_to = move_aside(path)?;
            Ok(StoredLoadOutcome::MovedAside { reason, moved_to })
        }
    }
}

fn encode_stored_queue(queue: &Queue, position_ms: u64) -> Result<String, QueueCodecError> {
    let encoded = queue.encode()?;
    let Some(body) = encoded
        .strip_prefix("{\"schemaVersion\":1,")
        .and_then(|text| text.strip_suffix('}'))
    else {
        return Err(QueueCodecError::Malformed);
    };
    let stored = format!(
        "{{\"schemaVersion\":{STORED_QUEUE_SCHEMA_VERSION},{body},\"position_ms\":{position_ms}}}"
    );
    if stored.len() > MAX_ENCODED_QUEUE_BYTES {
        return Err(QueueCodecError::TooLarge);
    }
    Ok(stored)
}

/// Remove the store-only offset field before passing the JSON to the stricter queue codec.
/// The scanner only identifies top-level members; `Queue::decode` validates the remaining JSON.
fn decode_stored_queue(text: &str) -> Result<StoredQueue, QueueCodecError> {
    let fields = top_level_fields(text)?;
    let mut schema_version = None;
    let mut position_ms = None;
    let mut retained = Vec::with_capacity(fields.len());
    for field in fields {
        if let Some(value) = parse_integer_field(field, "schemaVersion")? {
            if schema_version.replace(value).is_some() {
                return Err(QueueCodecError::Malformed);
            }
        } else if let Some(value) = parse_integer_field(field, "position_ms")? {
            if position_ms.replace(value).is_some() {
                return Err(QueueCodecError::Malformed);
            }
        } else {
            retained.push(field);
        }
    }
    let schema_version = schema_version.ok_or(QueueCodecError::Malformed)?;
    match schema_version {
        1 if position_ms.is_some() => return Err(QueueCodecError::Malformed),
        1 => {}
        STORED_QUEUE_SCHEMA_VERSION if position_ms.is_none() => {
            return Err(QueueCodecError::Malformed);
        }
        STORED_QUEUE_SCHEMA_VERSION => {}
        other => return Err(QueueCodecError::UnknownVersion(other)),
    }
    let queue_text = format!("{{\"schemaVersion\":1,{}}}", retained.join(","));
    let queue = Queue::decode(&queue_text)?;
    Ok(StoredQueue {
        queue,
        position_ms: position_ms.unwrap_or(0),
    })
}

/// Parse a top-level integer field used by the store version and playback offset.
fn parse_integer_field(field: &str, name: &str) -> Result<Option<u64>, QueueCodecError> {
    let field = field.trim();
    let key = format!("\"{name}\"");
    let Some(after_quote) = field.strip_prefix(&key) else {
        return Ok(None);
    };
    let value = after_quote
        .trim_start()
        .strip_prefix(':')
        .ok_or(QueueCodecError::Malformed)?
        .trim();
    if value.is_empty()
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return Err(QueueCodecError::Malformed);
    }
    let number = value.parse().map_err(|_| QueueCodecError::Malformed)?;
    Ok(Some(number))
}

/// Split an object into top-level members, respecting quoted strings and nested values.
fn top_level_fields(text: &str) -> Result<Vec<&str>, QueueCodecError> {
    let bytes = text.as_bytes();
    if bytes.first() != Some(&b'{') || bytes.last() != Some(&b'}') {
        return Err(QueueCodecError::Malformed);
    }
    let mut fields = Vec::new();
    let mut start = 1;
    let mut depth = 0_i32;
    let mut quoted = false;
    let mut escaped = false;
    for (index, byte) in bytes.iter().enumerate().take(bytes.len() - 1).skip(1) {
        if quoted {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                quoted = false;
            }
            continue;
        }
        match *byte {
            b'"' => quoted = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth -= 1;
                if depth < 0 {
                    return Err(QueueCodecError::Malformed);
                }
            }
            b',' if depth == 0 => {
                fields.push(text[start..index].trim());
                start = index + 1;
            }
            _ => {}
        }
    }
    if quoted || escaped || depth != 0 {
        return Err(QueueCodecError::Malformed);
    }
    let last = text[start..text.len() - 1].trim();
    if !last.is_empty() {
        fields.push(last);
    } else if start != 1 || !fields.is_empty() {
        return Err(QueueCodecError::Malformed);
    }
    Ok(fields)
}

/// Read at most one byte more than the codec accepts, so an oversized file is detected without
/// being read whole.
fn read_capped(path: &Path) -> io::Result<Vec<u8>> {
    let file = File::open(path)?;
    let mut bytes = Vec::new();
    file.take(MAX_ENCODED_QUEUE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Write `text` to `temp`, flush it to disk, then rename it over `path`.
///
/// `temp` must not exist yet. It is removed if any later step fails, but only after this
/// function has created it, so an existing file at `temp` is never deleted.
fn write_atomic(path: &Path, text: &str, temp: &Path) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(temp)?;
    let written = file
        .write_all(text.as_bytes())
        .and_then(|()| file.sync_all());
    // The handle must be closed before the file can be removed or renamed on Windows.
    drop(file);
    if let Err(err) = written.and_then(|()| fs::rename(temp, path)) {
        let _ = fs::remove_file(temp);
        return Err(err);
    }
    Ok(())
}

/// A name for the temporary file: hidden, in the target's directory, and unlikely to collide
/// with another writer. It combines the process ID, the time, and a counter.
fn temp_file_name(file_name: &std::ffi::OsStr) -> OsString {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let count = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut name = OsString::from(".");
    name.push(file_name);
    name.push(format!(
        ".{:x}-{:x}-{:x}.tmp",
        std::process::id(),
        nanos,
        count
    ));
    name
}

/// Rename the file at `path` to the first free `.corrupt` name next to it.
fn move_aside(path: &Path) -> Result<PathBuf, QueueStoreError> {
    let destination = free_corrupt_name(path)?;
    fs::rename(path, &destination)?;
    Ok(destination)
}

/// `<path>.corrupt`, or `<path>.corrupt.<n>` for the first `n` that is not taken, so that an
/// earlier moved-aside file is never overwritten.
fn free_corrupt_name(path: &Path) -> io::Result<PathBuf> {
    let first = with_suffix(path, CORRUPT_SUFFIX);
    if !first.exists() {
        return Ok(first);
    }
    for n in 1..=MAX_ASIDE_ATTEMPTS {
        let candidate = with_suffix(path, &format!("{CORRUPT_SUFFIX}.{n}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no free name for the moved-aside queue file",
    ))
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use jellyfin_api::session_reporting::{QueueItem, RepeatMode};

    use super::{
        LoadOutcome, QueueStoreError, StoredLoadOutcome, decode_stored_queue, load_queue,
        load_queue_with_position, save_queue, save_queue_with_position, write_atomic,
    };
    use crate::queue::{MAX_ENCODED_QUEUE_BYTES, Queue, QueueCodecError};

    /// A directory under the system temp directory, unique to one test. It is removed when the
    /// guard is dropped, including when the test panics.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos());
            let path = std::env::temp_dir().join(format!(
                "kattri-queue-store-{name}-{}-{nanos:x}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create test directory");
            Self(path)
        }

        fn file(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn item(n: usize) -> QueueItem {
        QueueItem::new(format!("{n:032x}"))
    }

    /// 200 items, repeat all, shuffle on, and the cursor advanced 57 steps into the play order.
    fn shuffled_queue() -> Queue {
        let mut queue = Queue::with_seed(7);
        for n in 0..200 {
            queue.add(item(n)).expect("add");
        }
        queue.set_repeat(RepeatMode::All);
        queue.set_shuffle(true);
        for _ in 0..57 {
            queue.next_track();
        }
        queue
    }

    fn list_ids(queue: &Queue) -> Vec<String> {
        queue.items().iter().map(|item| item.id.clone()).collect()
    }

    fn assert_same_state(original: &Queue, restored: &Queue) {
        assert_eq!(list_ids(original), list_ids(restored));
        assert_eq!(original.play_order(), restored.play_order());
        assert_eq!(original.current_index(), restored.current_index());
        assert_eq!(original.repeat(), restored.repeat());
        assert_eq!(original.is_shuffled(), restored.is_shuffled());
    }

    fn expect_loaded(outcome: LoadOutcome) -> Queue {
        match outcome {
            LoadOutcome::Loaded(queue) => queue,
            other => panic!("expected a loaded queue, got {other:?}"),
        }
    }

    /// Names left in `dir`, sorted, so tests can check for stray temporary files.
    fn names_in(dir: &TestDir) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&dir.0)
            .expect("read test directory")
            .map(|entry| {
                entry
                    .expect("directory entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    #[test]
    fn two_hundred_item_queue_with_cursor_and_shuffle_round_trips() {
        let dir = TestDir::new("round-trip");
        let path = dir.file("queue.json");
        let queue = shuffled_queue();

        save_queue(&path, &queue).expect("save");
        let mut restored = expect_loaded(load_queue(&path).expect("load"));

        assert_eq!(restored.len(), 200);
        assert_same_state(&queue, &restored);
        let mut original = queue;
        for _ in 0..400 {
            assert_eq!(restored.next_track(), original.next_track());
        }
        assert_same_state(&original, &restored);
    }

    #[test]
    fn position_aware_store_round_trips_offset_shuffle_repeat_and_cursor() {
        let dir = TestDir::new("position-round-trip");
        let path = dir.file("queue.json");
        let queue = shuffled_queue();

        save_queue_with_position(&path, &queue, 987_654).expect("save queue snapshot");
        let stored = match load_queue_with_position(&path).expect("load queue snapshot") {
            StoredLoadOutcome::Loaded(stored) => stored,
            other => panic!("expected a loaded queue snapshot, got {other:?}"),
        };

        assert_eq!(stored.position_ms, 987_654);
        assert_same_state(&queue, &stored.queue);
    }

    #[test]
    fn legacy_queue_without_position_restores_at_zero() {
        let legacy = "{\"schemaVersion\":1,\"repeat\":\"off\",\"shuffle\":false,\"items\":[],\"order\":[],\"cursor\":null,\"detached\":null}";
        let stored = decode_stored_queue(legacy).expect("decode legacy queue");
        assert_eq!(stored.position_ms, 0);
        assert!(stored.queue.is_empty());
    }

    #[test]
    fn persisted_schema_contains_position_shuffle_repeat_and_no_sensitive_strings() {
        let dir = TestDir::new("schema-guard");
        let path = dir.file("queue.json");
        let mut queue = shuffled_queue();
        queue.set_repeat(RepeatMode::All);
        save_queue_with_position(&path, &queue, 42).expect("save");
        let text = std::fs::read_to_string(path).expect("read persisted queue");

        assert!(text.contains("\"position_ms\":42"));
        assert!(text.starts_with("{\"schemaVersion\":2,"));
        assert!(text.contains("\"shuffle\":true"));
        assert!(text.contains("\"repeat\":\"all\""));
        let lowercase = text.to_ascii_lowercase();
        for forbidden in [
            "http",
            "api_key",
            "apikey",
            "token",
            "password",
            "authorization",
            "bearer",
            "credential",
            "secret",
            "://",
        ] {
            assert!(
                !lowercase.contains(forbidden),
                "persisted queue contains forbidden text {forbidden}"
            );
        }
    }

    #[test]
    fn duplicate_or_invalid_position_is_rejected() {
        let queue = Queue::new().encode().expect("encode queue");
        let body = queue
            .strip_prefix("{\"schemaVersion\":1,")
            .and_then(|text| text.strip_suffix('}'))
            .expect("queue fields");
        for suffix in [
            ",\"position_ms\":1,\"position_ms\":2}",
            ",\"position_ms\":-1}",
            ",\"position_ms\":01}",
            ",\"position_ms\":18446744073709551616}",
        ] {
            let text = format!("{{\"schemaVersion\":2,{body}{suffix}");
            assert_eq!(
                decode_stored_queue(&text).err(),
                Some(QueueCodecError::Malformed),
                "{text}"
            );
        }
    }

    #[test]
    fn save_over_an_existing_file_replaces_it_and_leaves_no_temp_file() {
        let dir = TestDir::new("overwrite");
        let path = dir.file("queue.json");
        save_queue(&path, &shuffled_queue()).expect("first save");

        let mut small = Queue::with_seed(1);
        small.add(item(5)).expect("add");
        save_queue(&path, &small).expect("second save");

        let restored = expect_loaded(load_queue(&path).expect("load"));
        assert_same_state(&small, &restored);
        assert_eq!(names_in(&dir), vec!["queue.json".to_owned()]);
    }

    #[test]
    fn save_creates_a_missing_parent_directory() {
        let dir = TestDir::new("parent");
        let path = dir.file("nested").join("queue.json");
        save_queue(&path, &shuffled_queue()).expect("save");
        assert!(matches!(
            load_queue(&path).expect("load"),
            LoadOutcome::Loaded(_)
        ));
    }

    #[test]
    fn missing_file_reports_missing() {
        let dir = TestDir::new("missing");
        let outcome = load_queue(&dir.file("queue.json")).expect("load");
        assert!(matches!(outcome, LoadOutcome::Missing), "{outcome:?}");
        assert!(names_in(&dir).is_empty());
    }

    #[test]
    fn position_aware_loader_reports_missing_file() {
        let dir = TestDir::new("position-missing");
        assert!(matches!(
            load_queue_with_position(&dir.file("queue.json")).expect("load"),
            StoredLoadOutcome::Missing
        ));
    }

    #[test]
    fn corrupt_file_is_moved_aside_and_reported_not_deleted() {
        let dir = TestDir::new("corrupt");
        let path = dir.file("queue.json");
        let garbage = b"{\"schemaVersion\":1, not json at all";
        std::fs::write(&path, garbage).expect("write garbage");

        match load_queue(&path).expect("load") {
            LoadOutcome::MovedAside { reason, moved_to } => {
                assert_eq!(reason, QueueCodecError::Malformed);
                assert_eq!(moved_to, dir.file("queue.json.corrupt"));
                assert_eq!(std::fs::read(&moved_to).expect("read aside"), garbage);
            }
            other => panic!("expected MovedAside, got {other:?}"),
        }
        assert!(!path.exists());
        assert_eq!(names_in(&dir), vec!["queue.json.corrupt".to_owned()]);
    }

    #[test]
    fn non_utf8_file_is_moved_aside() {
        let dir = TestDir::new("non-utf8");
        let path = dir.file("queue.json");
        std::fs::write(&path, [0xff_u8, 0xfe, 0x00, 0x7b]).expect("write bytes");

        match load_queue(&path).expect("load") {
            LoadOutcome::MovedAside { reason, moved_to } => {
                assert_eq!(reason, QueueCodecError::Malformed);
                assert!(moved_to.exists());
            }
            other => panic!("expected MovedAside, got {other:?}"),
        }
        assert!(!path.exists());
    }

    #[test]
    fn unknown_version_is_moved_aside_with_the_version_found() {
        let dir = TestDir::new("version");
        let path = dir.file("queue.json");
        let text = "{\"schemaVersion\":3,\"repeat\":\"off\",\"shuffle\":false,\"items\":[],\"order\":[],\"cursor\":null,\"detached\":null}";
        std::fs::write(&path, text).expect("write future version");

        match load_queue(&path).expect("load") {
            LoadOutcome::MovedAside { reason, moved_to } => {
                assert_eq!(reason, QueueCodecError::UnknownVersion(3));
                assert_eq!(moved_to, dir.file("queue.json.corrupt"));
                assert_eq!(std::fs::read_to_string(&moved_to).expect("read"), text);
            }
            other => panic!("expected MovedAside, got {other:?}"),
        }
        assert!(!path.exists());
    }

    #[test]
    fn oversized_file_is_moved_aside_without_decoding_it_whole() {
        let dir = TestDir::new("oversized");
        let path = dir.file("queue.json");
        let oversized = vec![b' '; MAX_ENCODED_QUEUE_BYTES + 1];
        std::fs::write(&path, oversized).expect("write oversized");

        match load_queue(&path).expect("load") {
            LoadOutcome::MovedAside { reason, .. } => {
                assert_eq!(reason, QueueCodecError::TooLarge);
            }
            other => panic!("expected MovedAside, got {other:?}"),
        }
        assert!(!path.exists());
    }

    #[test]
    fn a_second_moved_aside_file_gets_a_numbered_name() {
        let dir = TestDir::new("second-aside");
        let path = dir.file("queue.json");
        std::fs::write(dir.file("queue.json.corrupt"), b"earlier").expect("write earlier");
        std::fs::write(&path, b"broken").expect("write broken");

        match load_queue(&path).expect("load") {
            LoadOutcome::MovedAside { moved_to, .. } => {
                assert_eq!(moved_to, dir.file("queue.json.corrupt.1"));
            }
            other => panic!("expected MovedAside, got {other:?}"),
        }
        assert_eq!(
            std::fs::read(dir.file("queue.json.corrupt")).expect("earlier kept"),
            b"earlier"
        );
    }

    #[test]
    fn failed_write_leaves_the_original_file_unchanged() {
        let dir = TestDir::new("failed-write");
        let path = dir.file("queue.json");
        let original = shuffled_queue();
        save_queue(&path, &original).expect("initial save");
        let before = std::fs::read(&path).expect("read original");

        // The temporary file cannot be created because its directory does not exist.
        let unreachable_temp = dir.file("missing").join(".queue.json.tmp");
        let mut replacement = Queue::with_seed(3);
        replacement.add(item(9)).expect("add");
        let text = replacement.encode().expect("encode");
        let result = write_atomic(&path, &text, &unreachable_temp);
        assert!(
            result.is_err(),
            "write should fail when the temp file cannot be created"
        );

        assert_eq!(std::fs::read(&path).expect("read after failure"), before);
        let restored = expect_loaded(load_queue(&path).expect("load"));
        assert_same_state(&original, &restored);
        assert_eq!(names_in(&dir), vec!["queue.json".to_owned()]);
    }

    #[test]
    fn unsafe_item_id_is_refused_before_anything_is_written() {
        let dir = TestDir::new("unsafe");
        let path = dir.file("queue.json");
        let mut queue = Queue::new();
        queue
            .add(QueueItem::new("https://media.example/Audio/1"))
            .expect("add");

        match save_queue(&path, &queue) {
            Err(QueueStoreError::Encode(QueueCodecError::UnsafeId)) => {}
            other => panic!("expected an unsafe ID error, got {other:?}"),
        }
        assert!(names_in(&dir).is_empty());
    }
}

//! M4-20 test that the PIN never reaches a file in the parental store directory in plain text.
//!
//! The PIN is set in memory. The test then writes what the app persists (the Argon2id record and
//! the attempt state) into one directory and scans every file there, byte for byte, for the
//! PIN digits. A positive control proves the scanner would catch a leak.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use kattri_parental::{ATTEMPT_FILE_NAME, HashParams, Pin, PinLock, Unlock, save_attempts};

/// Eight digits, so a match inside the hex text of a record is not a realistic accident.
const TEST_PIN: &str = "73918264";
/// Stands in for the settings entry, which kattri-settings writes outside this crate.
const RECORD_FILE_NAME: &str = "pin.record";

/// A temp directory removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "kattri-parental-{label}-{}-{nanos}-{n}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Cheapest valid Argon2 cost, to keep the suite fast. Production uses `OWASP_MINIMUM`.
fn fast() -> HashParams {
    match HashParams::new(8, 1, 1) {
        Ok(p) => p,
        Err(err) => panic!("test params should be valid: {err}"),
    }
}

/// Every regular file under `dir`, recursively.
fn files_under(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            out.extend(files_under(&path)?);
        } else {
            out.push(path);
        }
    }
    Ok(out)
}

/// Paths of the files under `dir` whose bytes contain `needle`.
fn files_containing(dir: &Path, needle: &[u8]) -> std::io::Result<Vec<PathBuf>> {
    let mut hits = Vec::new();
    for path in files_under(dir)? {
        let bytes = fs::read(&path)?;
        if bytes.windows(needle.len()).any(|window| window == needle) {
            hits.push(path);
        }
    }
    Ok(hits)
}

#[test]
fn pin_is_not_in_plaintext_in_any_store_file() {
    let dir = TempDir::new("secrecy");
    let pin = match Pin::parse(TEST_PIN) {
        Ok(p) => p,
        Err(err) => panic!("test PIN should parse: {err}"),
    };

    let mut lock = PinLock::new(None);
    if let Err(err) = lock.set_pin(&pin, fast(), [3u8; 16]) {
        panic!("set_pin should succeed: {err}");
    }
    let record = match lock.stored() {
        Some(stored) => stored.to_record(),
        None => panic!("set_pin should store a PIN record"),
    };
    let record_path = dir.path().join(RECORD_FILE_NAME);
    fs::write(&record_path, format!("{record}\n")).expect("write record");

    // A wrong attempt and then the right one, so both persisted states are written.
    let attempts_path = dir.path().join(ATTEMPT_FILE_NAME);
    if let Err(err) = lock.try_unlock("00000000", 0) {
        panic!("wrong attempt should not error: {err}");
    }
    save_attempts(&attempts_path, lock.attempt_state()).expect("save failed attempt");
    assert_eq!(lock.try_unlock(TEST_PIN, 10), Ok(Unlock::Unlocked));
    save_attempts(&attempts_path, lock.attempt_state()).expect("save cleared state");

    let files = files_under(dir.path()).expect("list store directory");
    assert!(
        files.len() >= 2,
        "expected the record and attempt files, found {files:?}"
    );
    let hits = files_containing(dir.path(), TEST_PIN.as_bytes()).expect("scan store directory");
    assert!(hits.is_empty(), "plaintext PIN found in: {hits:?}");
}

#[test]
fn scanner_finds_a_plaintext_pin_it_is_given() {
    let dir = TempDir::new("control");
    fs::write(dir.path().join("leak.txt"), format!("pin = {TEST_PIN}")).expect("write control");
    let hits = files_containing(dir.path(), TEST_PIN.as_bytes()).expect("scan control directory");
    assert_eq!(hits.len(), 1, "scanner should find the planted PIN");
}

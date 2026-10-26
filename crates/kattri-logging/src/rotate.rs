//! A size-bounded log file: `kattri.log` plus `kattri.log.1` .. `kattri.log.N`.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Active log file name. Rotated files append `.1`, `.2`, and so on; `.1` is the newest.
pub const LOG_FILE_NAME: &str = "kattri.log";

pub(crate) struct RotatingFile {
    dir: PathBuf,
    max_bytes: u64,
    max_files: usize,
    file: File,
    written: u64,
}

impl RotatingFile {
    /// Opens (creating the directory) the active file in append mode. `max_files` counts the
    /// active file too and is at least 1; `max_bytes` is at least 1 KiB.
    pub(crate) fn open(dir: &Path, max_bytes: u64, max_files: usize) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let file = open_append(&dir.join(LOG_FILE_NAME))?;
        let written = file.metadata()?.len();
        Ok(Self {
            dir: dir.to_owned(),
            max_bytes: max_bytes.max(1024),
            max_files: max_files.max(1),
            file,
            written,
        })
    }

    /// Appends one complete line (the caller includes the newline). Rotation happens before
    /// the write when the line would push the file past the limit. A single line larger than
    /// the configured limit is dropped so it cannot make a log file exceed that limit.
    pub(crate) fn write_line(&mut self, line: &[u8]) -> io::Result<()> {
        let line_bytes = line.len() as u64;
        if line_bytes > self.max_bytes {
            return Ok(());
        }
        if self.written > 0 && self.written.saturating_add(line_bytes) > self.max_bytes {
            self.rotate()?;
        }
        self.file.write_all(line)?;
        self.written = self.written.saturating_add(line_bytes);
        Ok(())
    }

    pub(crate) fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file.flush()?;
        let active = self.dir.join(LOG_FILE_NAME);
        if self.max_files == 1 {
            // No history is kept: start the active file over.
            self.file = OpenOptions::new()
                .write(true)
                .truncate(true)
                .create(true)
                .open(&active)?;
        } else {
            let oldest = numbered(&self.dir, self.max_files - 1);
            let _ = fs::remove_file(&oldest);
            for n in (1..self.max_files - 1).rev() {
                let from = numbered(&self.dir, n);
                if from.exists() {
                    fs::rename(&from, numbered(&self.dir, n + 1))?;
                }
            }
            fs::rename(&active, numbered(&self.dir, 1))?;
            self.file = open_append(&active)?;
        }
        self.written = 0;
        Ok(())
    }
}

fn numbered(dir: &Path, n: usize) -> PathBuf {
    dir.join(format!("{LOG_FILE_NAME}.{n}"))
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kattri-logging-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn line(n: usize) -> Vec<u8> {
        format!("{n:04} {}\n", "x".repeat(100)).into_bytes()
    }

    #[test]
    fn a_small_log_stays_in_one_file() {
        let dir = scratch("small");
        let mut log = RotatingFile::open(&dir, 1024 * 1024, 3).expect("open");
        for n in 0..5 {
            log.write_line(&line(n)).expect("write");
        }
        log.flush().expect("flush");
        assert!(dir.join(LOG_FILE_NAME).exists());
        assert!(!numbered(&dir, 1).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn full_file_rotates_and_the_oldest_is_dropped() {
        let dir = scratch("rotate");
        // 1 KiB limit holds nine 105-byte lines; 100 lines need many rotations.
        let mut log = RotatingFile::open(&dir, 1024, 3).expect("open");
        for n in 0..100 {
            log.write_line(&line(n)).expect("write");
        }
        log.flush().expect("flush");
        let mut names: Vec<String> = fs::read_dir(&dir)
            .expect("read dir")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        assert_eq!(names, ["kattri.log", "kattri.log.1", "kattri.log.2"]);
        for name in &names {
            let len = fs::metadata(dir.join(name)).expect("metadata").len();
            assert!(len <= 1024, "{name} is {len} bytes");
        }
        // The newest line is in the active file, and older lines are in higher numbers.
        let active = fs::read_to_string(dir.join(LOG_FILE_NAME)).expect("read active");
        assert!(active.contains("0099 "), "{active}");
        let first = fs::read_to_string(numbered(&dir, 1)).expect("read .1");
        let second = fs::read_to_string(numbered(&dir, 2)).expect("read .2");
        assert!(first.contains("0090 ") && !first.contains("0099 "));
        assert!(second.lines().next().unwrap_or("") < first.lines().next().unwrap_or(""));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_single_file_limit_keeps_no_history() {
        let dir = scratch("single");
        let mut log = RotatingFile::open(&dir, 1024, 1).expect("open");
        for n in 0..50 {
            log.write_line(&line(n)).expect("write");
        }
        log.flush().expect("flush");
        assert!(!numbered(&dir, 1).exists());
        assert!(
            fs::metadata(dir.join(LOG_FILE_NAME))
                .expect("metadata")
                .len()
                <= 1024
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_oversized_line_does_not_exceed_the_file_limit() {
        let dir = scratch("oversized-line");
        let mut log = RotatingFile::open(&dir, 1024, 2).expect("open");
        log.write_line(&vec![b'x'; 1025])
            .expect("oversized line is dropped");
        log.flush().expect("flush");
        assert_eq!(
            fs::metadata(dir.join(LOG_FILE_NAME))
                .expect("metadata")
                .len(),
            0
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reopening_appends_and_counts_the_existing_size() {
        let dir = scratch("reopen");
        {
            let mut log = RotatingFile::open(&dir, 1024, 2).expect("open");
            for n in 0..8 {
                log.write_line(&line(n)).expect("write");
            }
        }
        let mut log = RotatingFile::open(&dir, 1024, 2).expect("reopen");
        log.write_line(&line(8)).expect("write");
        log.write_line(&line(9)).expect("write");
        log.flush().expect("flush");
        // 8 * 105 bytes were already there, so the next lines forced a rotation.
        assert!(numbered(&dir, 1).exists());
        let _ = fs::remove_dir_all(&dir);
    }
}

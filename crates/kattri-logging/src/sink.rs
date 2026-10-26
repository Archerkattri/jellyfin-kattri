//! The redacting, line-oriented writer between `tracing-subscriber` and the rotating file.

use std::io::{self, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tracing_subscriber::fmt::MakeWriter;

use crate::rotate::RotatingFile;

/// An unfinished formatter line must not let a broken writer grow memory without bound.
const MAX_PENDING_LINE_BYTES: usize = 64 * 1024;

struct Inner {
    file: RotatingFile,
}

pub(crate) struct Sink {
    inner: Mutex<Inner>,
}

impl Sink {
    pub(crate) fn open(dir: &Path, max_bytes: u64, max_files: usize) -> io::Result<Self> {
        Ok(Self {
            inner: Mutex::new(Inner {
                file: RotatingFile::open(dir, max_bytes, max_files)?,
            }),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A panic while logging must not stop later logging, so a poisoned lock is reused.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn write_redacted_line(&self, line: &str) -> io::Result<()> {
        let mut inner = self.lock();
        inner.file.write_line(line.as_bytes())
    }

    fn flush(&self) -> io::Result<()> {
        self.lock().file.flush()
    }

    /// Flushes the file. Each event writer commits its own unfinished line when it is dropped,
    /// so shutdown does not need to combine partial writes from unrelated events.
    pub(crate) fn finish(&self) {
        let _ = self.flush();
    }
}

/// Splits `<timestamp> <LEVEL> <target>:` from the message. The prefix is written by the
/// formatter, not by callers, and a module name such as `auth` or `secret` would otherwise make
/// the redactor drop the whole line. When the line does not start with a timestamp and a level,
/// nothing is split off and the whole line is redacted.
fn split_prefix(line: &str) -> (&str, &str) {
    let mut end = 0;
    let mut parts = 0;
    let mut rest = line;
    while parts < 3 {
        let trimmed = rest.trim_start_matches(' ');
        let skipped = rest.len() - trimmed.len();
        let word_len = trimmed.find(' ').unwrap_or(trimmed.len());
        let word = &trimmed[..word_len];
        let ok = match parts {
            0 => is_timestamp(word),
            1 => matches!(word, "ERROR" | "WARN" | "INFO" | "DEBUG" | "TRACE"),
            _ => is_target(word),
        };
        if !ok {
            break;
        }
        end += skipped + word_len;
        rest = &trimmed[word_len..];
        parts += 1;
    }
    // Timestamp and level are required; the target is optional.
    if parts < 2 {
        return ("", line);
    }
    line.split_at(end)
}

fn is_timestamp(word: &str) -> bool {
    word.len() >= 20
        && word.ends_with('Z')
        && word.as_bytes().get(10) == Some(&b'T')
        && word
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '-' | ':' | '.' | 'T' | 'Z'))
}

fn is_target(word: &str) -> bool {
    word.len() > 1
        && word.ends_with(':')
        && word[..word.len() - 1]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
}

/// Cheap handle that `tracing-subscriber` clones for every event.
pub(crate) struct SinkWriter {
    sink: Arc<Sink>,
    /// Per-event unfinished line and credential continuation state.
    pending: Vec<u8>,
    /// An oversized line invalidates context for the rest of this event, so discard its tail.
    dropping_event: bool,
    formatter_prefix_pending: bool,
    redactor: kattri_guard::Redactor,
}

impl Clone for SinkWriter {
    fn clone(&self) -> Self {
        Self::new(Arc::clone(&self.sink))
    }
}

impl SinkWriter {
    pub(crate) fn new(sink: Arc<Sink>) -> Self {
        Self {
            sink,
            pending: Vec::new(),
            dropping_event: false,
            formatter_prefix_pending: true,
            redactor: kattri_guard::Redactor::default(),
        }
    }

    fn write_bytes(&mut self, mut rest: &[u8]) -> io::Result<()> {
        while !rest.is_empty() {
            if self.dropping_event {
                break;
            } else if let Some(end) = rest.iter().position(|byte| *byte == b'\n') {
                let line_len = end + 1;
                if self.pending.len() + line_len > MAX_PENDING_LINE_BYTES {
                    self.pending.clear();
                    self.dropping_event = true;
                    break;
                } else {
                    self.pending.extend_from_slice(&rest[..line_len]);
                    self.emit_pending()?;
                }
                rest = &rest[line_len..];
            } else if self.pending.len() + rest.len() > MAX_PENDING_LINE_BYTES {
                self.pending.clear();
                self.dropping_event = true;
                break;
            } else {
                self.pending.extend_from_slice(rest);
                break;
            }
        }
        Ok(())
    }

    fn emit_pending(&mut self) -> io::Result<()> {
        let bytes = std::mem::take(&mut self.pending);
        let text = String::from_utf8_lossy(&bytes);
        let body = text.trim_end_matches(['\r', '\n']);
        // Only the first physical line has a formatter-owned prefix. A later line is
        // message content, even if it imitates a timestamp, level and module target.
        let (prefix, message) = if std::mem::replace(&mut self.formatter_prefix_pending, false) {
            split_prefix(body)
        } else {
            ("", body)
        };
        let mut out = String::with_capacity(body.len() + 1);
        out.push_str(prefix);
        out.push_str(&self.redactor.redact(message));
        out.push('\n');
        self.sink.write_redacted_line(&out)
    }

    fn finish_pending(&mut self) {
        if !self.pending.is_empty() && !self.dropping_event {
            self.pending.push(b'\n');
            let _ = self.emit_pending();
        }
        let _ = self.sink.flush();
    }
}

impl Write for SinkWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // A full disk or a removed folder must not take the app down: drop the line, report ok.
        let _ = self.write_bytes(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // Flush is a boundary for this event writer, so finalize only its own partial line.
        self.finish_pending();
        Ok(())
    }
}

impl Drop for SinkWriter {
    fn drop(&mut self) {
        self.finish_pending();
    }
}

impl<'a> MakeWriter<'a> for SinkWriter {
    type Writer = SinkWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kattri-sink-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn read(dir: &Path) -> String {
        fs::read_to_string(dir.join(crate::LOG_FILE_NAME)).unwrap_or_default()
    }

    #[test]
    fn lines_are_redacted_before_they_reach_the_file() {
        let dir = scratch("redact");
        let sink = Arc::new(Sink::open(&dir, 1024 * 1024, 2).expect("open"));
        let mut writer = SinkWriter::new(Arc::clone(&sink));
        writer
            .write_all(b"GET /Items?api_key=SECRETVALUE1 ok\nsecond line\n")
            .expect("write");
        let text = read(&dir);
        assert!(!text.contains("SECRETVALUE1"), "{text}");
        assert!(text.contains("second line\n"), "{text}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_formatter_prefix_is_not_redacted_but_the_message_is() {
        let line = "2026-10-23T15:33:19.924850Z  INFO jellyfin_api::auth: signed in token=abc";
        let (prefix, message) = split_prefix(line);
        assert_eq!(
            prefix,
            "2026-10-23T15:33:19.924850Z  INFO jellyfin_api::auth:"
        );
        assert_eq!(message, " signed in token=abc");
        // No target: the message starts right after the level.
        let (prefix, _) = split_prefix("2026-10-23T15:33:19.924850Z  WARN plain words");
        assert_eq!(prefix, "2026-10-23T15:33:19.924850Z  WARN");
    }

    #[test]
    fn lines_without_the_expected_prefix_are_fully_redacted() {
        for line in [
            "token=abc",
            "INFO 2026-10-23T15:33:19.924850Z x",
            "2026-10-23T15:33:19.924850Z LOUD x",
            "not a timestamp INFO target: x",
        ] {
            assert_eq!(split_prefix(line).0, "", "{line}");
        }
    }

    #[test]
    fn a_secret_in_the_target_position_cannot_hide_in_the_prefix() {
        let line = "2026-10-23T15:33:19.924850Z  INFO password=hunter22 x";
        let (prefix, message) = split_prefix(line);
        assert_eq!(prefix, "2026-10-23T15:33:19.924850Z  INFO");
        assert!(message.contains("password=hunter22"));
    }

    #[test]
    fn a_line_split_across_writes_is_redacted_as_one_line() {
        let dir = scratch("split");
        let sink = Arc::new(Sink::open(&dir, 1024 * 1024, 2).expect("open"));
        let mut writer = SinkWriter::new(Arc::clone(&sink));
        writer.write_all(b"login pass").expect("write");
        writer.write_all(b"word=hunter22 done\n").expect("write");
        let text = read(&dir);
        assert!(!text.contains("hunter22"), "{text}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversized_lines_discard_the_rest_of_the_event_with_bounded_memory() {
        let dir = scratch("oversized-pending");
        let sink = Arc::new(Sink::open(&dir, 1024 * 1024, 2).expect("open"));
        let mut writer = SinkWriter::new(Arc::clone(&sink));
        writer
            .write_all(&vec![b'x'; MAX_PENDING_LINE_BYTES + 1])
            .expect("write");
        assert_eq!(writer.pending.len(), 0);
        assert!(writer.dropping_event);

        // Keep discarding this event: redaction context may have been lost with its long line.
        writer
            .write_all(b"SECRET-tail\nnext event\n")
            .expect("write");
        drop(writer);
        assert!(!read(&dir).contains("SECRET-tail"));
        assert!(!read(&dir).contains("next event"));

        // A separate event remains writable after the oversized event was discarded.
        let mut next_event = SinkWriter::new(Arc::clone(&sink));
        next_event.write_all(b"independent event\n").expect("write");
        assert!(read(&dir).ends_with("independent event\n"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn finish_writes_an_unfinished_line() {
        let dir = scratch("finish");
        let sink = Arc::new(Sink::open(&dir, 1024 * 1024, 2).expect("open"));
        let mut writer = SinkWriter::new(Arc::clone(&sink));
        writer.write_all(b"last words").expect("write");
        assert_eq!(read(&dir), "");
        // Event writers finalize their own partial line on drop; shutdown then flushes the file.
        drop(writer);
        sink.finish();
        assert_eq!(read(&dir), "last words\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn continuation_text_cannot_impersonate_a_formatter_prefix_to_hide_a_secret_key() {
        let dir = scratch("forged-continuation-prefix");
        let sink = Arc::new(Sink::open(&dir, 1024 * 1024, 2).expect("open"));
        let mut writer = SinkWriter::new(Arc::clone(&sink));
        writer
            .write_all(
                b"2026-10-23T15:33:19Z INFO jellyfin_api: server error\n2026-10-23T15:33:19Z INFO password: short-secret\n",
            )
            .expect("write");
        drop(writer);
        let text = read(&dir);
        assert!(text.contains("jellyfin_api: server error"), "{text}");
        assert!(!text.contains("short-secret"), "{text}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn multiline_credentials_with_blank_and_scheme_lines_are_redacted_by_the_file_sink() {
        let dir = scratch("multiline-credential");
        let sink = Arc::new(Sink::open(&dir, 1024 * 1024, 2).expect("open"));
        let mut writer = SinkWriter::new(Arc::clone(&sink));
        writer
            .write_all(b"Authorization:\n\nBearer\n\nshort-secret\nnext event\n")
            .expect("write");
        drop(writer);

        let text = read(&dir);
        assert!(!text.contains("short-secret"), "{text}");
        assert!(text.contains("next event\n"), "{text}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn authorization_bearer_line_keeps_continuation_through_to_the_value() {
        let dir = scratch("authorization-bearer-continuation");
        let sink = Arc::new(Sink::open(&dir, 1024 * 1024, 2).expect("open"));
        let mut writer = SinkWriter::new(Arc::clone(&sink));
        writer
            .write_all(b"Authorization: Bearer\nshort-secret\n")
            .expect("write");
        drop(writer);

        let text = read(&dir);
        assert!(!text.contains("short-secret"), "{text}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversized_credential_line_cannot_leak_a_later_short_value() {
        let dir = scratch("oversized-credential-context");
        let sink = Arc::new(Sink::open(&dir, 1024 * 1024, 2).expect("open"));
        let mut writer = SinkWriter::new(Arc::clone(&sink));
        writer
            .write_all(b"Authorization: Bearer ")
            .expect("credential header");
        writer
            .write_all(&vec![b'x'; MAX_PENDING_LINE_BYTES])
            .expect("oversized credential line");
        writer
            .write_all(b"\nshort-secret\n")
            .expect("credential continuation");
        drop(writer);

        let text = read(&dir);
        assert!(!text.contains("short-secret"), "{text}");
        assert!(
            text.is_empty(),
            "dropped event must not be partially persisted: {text}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn make_writer_interleaving_keeps_partial_credentials_with_their_event() {
        let dir = scratch("interleaved-events");
        let sink = Arc::new(Sink::open(&dir, 1024 * 1024, 2).expect("open"));
        let factory = SinkWriter::new(Arc::clone(&sink));
        let mut credential_event = factory.make_writer();
        let mut other_event = factory.make_writer();

        credential_event
            .write_all(b"Authorization:")
            .expect("credential prefix");
        other_event.write_all(b"\n").expect("other event newline");
        credential_event
            .write_all(b"\nshort-secret\n")
            .expect("credential value");
        drop(credential_event);
        drop(other_event);

        let text = read(&dir);
        assert!(!text.contains("short-secret"), "{text}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_utf8_does_not_panic_or_stop_logging() {
        let dir = scratch("utf8");
        let sink = Arc::new(Sink::open(&dir, 1024 * 1024, 2).expect("open"));
        let mut writer = SinkWriter::new(Arc::clone(&sink));
        writer
            .write_all(b"bad \xff\xfe bytes\nnext\n")
            .expect("write");
        assert!(read(&dir).ends_with("next\n"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_folder_does_not_make_writes_fail() {
        let dir = scratch("gone");
        let sink = Arc::new(Sink::open(&dir, 1024, 2).expect("open"));
        let mut writer = SinkWriter::new(Arc::clone(&sink));
        let _ = fs::remove_dir_all(&dir);
        // Rotation needs the folder; the write is dropped and still reports success.
        for _ in 0..40 {
            assert!(writer.write_all(&[b'x'; 100]).is_ok());
            assert!(writer.write_all(b"\n").is_ok());
        }
    }
}

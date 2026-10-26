//! Production log file for Jellyfin Kattri.
//!
//! - Lines go to `<data-local-dir>/logs/kattri.log` ([`log_directory`]). The file is bounded:
//!   when it reaches [`LogConfig::max_file_bytes`] it becomes `kattri.log.1`, older files shift
//!   up, and the oldest of [`LogConfig::max_files`] is deleted.
//! - Every line passes [`kattri_guard::redact`] before it is written, so a token, `api_key`,
//!   `Authorization` header, password, or URL that reaches a log call never reaches the file.
//!   Callers should still log state names and counts, not credentials; this is the second guard.
//! - The level comes from the `KATTRI_LOG` environment variable (`error`, `warn`, `info`,
//!   `debug`, `trace`, `off`), then the caller's setting, then `info` ([`resolve_level`]).
//! - Panics are logged with their location by [`install_panic_hook`].
//! - Nothing is uploaded. The logging subscriber has no network code.
//!
//! The format is plain text, one event per line. JSON lines are not offered: the shared
//! redactor works on whitespace-separated tokens and would drop a whole compact JSON line.

#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::todo,
        clippy::unimplemented
    )
)]

mod rotate;
mod sink;

use std::any::Any;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, OnceLock};

use directories::ProjectDirs;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::Layer;
use tracing_subscriber::Registry;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::reload;

pub use rotate::LOG_FILE_NAME;
use sink::Sink;

/// Environment variable that sets the log level.
pub const LEVEL_ENV_VAR: &str = "KATTRI_LOG";

/// Default size at which the active file rotates.
pub const DEFAULT_MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// Default number of files kept, counting the active one.
pub const DEFAULT_MAX_FILES: usize = 5;

/// Folder holding the log files, or `None` when the platform has no per-user data folder.
/// The About page shows this path.
pub fn log_directory() -> Option<PathBuf> {
    ProjectDirs::from("org", "Jellyfin", "Kattri").map(|dirs| dirs.data_local_dir().join("logs"))
}

/// Folder for local crash summaries: a subfolder of [`log_directory`], so one folder holds
/// everything a tester needs to share.
pub fn crash_directory() -> Option<PathBuf> {
    log_directory().map(|dir| dir.join("crashes"))
}

/// Log verbosity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogLevel {
    Off,
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    fn filter(self) -> LevelFilter {
        match self {
            Self::Off => LevelFilter::OFF,
            Self::Error => LevelFilter::ERROR,
            Self::Warn => LevelFilter::WARN,
            Self::Info => LevelFilter::INFO,
            Self::Debug => LevelFilter::DEBUG,
            Self::Trace => LevelFilter::TRACE,
        }
    }
}

/// The text was not a level name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown log level")]
pub struct ParseLevelError;

impl FromStr for LogLevel {
    type Err = ParseLevelError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text.trim().to_ascii_lowercase().as_str() {
            "off" | "none" => Ok(Self::Off),
            "error" => Ok(Self::Error),
            "warn" | "warning" => Ok(Self::Warn),
            "info" => Ok(Self::Info),
            "debug" => Ok(Self::Debug),
            "trace" => Ok(Self::Trace),
            _ => Err(ParseLevelError),
        }
    }
}

/// The level to use: the environment value if it parses, else the setting if it parses, else
/// `info`. An unparseable value is ignored rather than turning logging off.
pub fn resolve_level(setting: Option<&str>, env: Option<&str>) -> LogLevel {
    env.and_then(|text| text.parse().ok())
        .or_else(|| setting.and_then(|text| text.parse().ok()))
        .unwrap_or_default()
}

/// How and where to log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogConfig {
    pub directory: PathBuf,
    pub level: LogLevel,
    pub max_file_bytes: u64,
    pub max_files: usize,
}

impl LogConfig {
    pub fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            level: LogLevel::default(),
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            max_files: DEFAULT_MAX_FILES,
        }
    }

    pub fn with_level(mut self, level: LogLevel) -> Self {
        self.level = level;
        self
    }
}

/// Why logging could not start.
#[derive(Debug, thiserror::Error)]
pub enum LogError {
    #[error("could not open the log file: {0}")]
    Io(#[from] std::io::Error),
    #[error("a global tracing subscriber is already installed")]
    AlreadyInitialized,
}

/// Handle that changes the level of the installed subscriber.
static LEVEL_HANDLE: OnceLock<reload::Handle<Targets, Registry>> = OnceLock::new();

/// Changes the log level of the running logger. Returns false when logging was not started or
/// the change could not be applied. Safe to call every frame: an unchanged level is a cheap
/// comparison the caller makes first (see [`current_level`]).
pub fn set_level(level: LogLevel) -> bool {
    let Some(handle) = LEVEL_HANDLE.get() else {
        return false;
    };
    if handle.reload(targets(level)).is_ok() {
        *CURRENT_LEVEL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(level);
        true
    } else {
        false
    }
}

/// The level the logger runs at now, or `None` when logging was not started.
pub fn current_level() -> Option<LogLevel> {
    *CURRENT_LEVEL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

static CURRENT_LEVEL: std::sync::Mutex<Option<LogLevel>> = std::sync::Mutex::new(None);

/// Keeps the log open. Dropping it writes any unfinished line and flushes the file.
#[must_use = "logging stops being flushed when the guard is dropped"]
pub struct LogGuard {
    sink: Arc<Sink>,
}

impl Drop for LogGuard {
    fn drop(&mut self) {
        self.sink.finish();
    }
}

/// Opens the log file and installs the global `tracing` subscriber.
///
/// Call once, early in `main`. Keep the returned guard alive until the program exits.
pub fn init(config: &LogConfig) -> Result<LogGuard, LogError> {
    let sink = Arc::new(Sink::open(
        &config.directory,
        config.max_file_bytes,
        config.max_files,
    )?);
    let (filter, handle) = reload::Layer::new(targets(config.level));
    let layer = tracing_subscriber::fmt::layer()
        .with_writer(sink::SinkWriter::new(Arc::clone(&sink)))
        .with_ansi(false)
        .with_filter(filter);
    let subscriber = tracing_subscriber::registry().with(layer);
    tracing::subscriber::set_global_default(subscriber)
        .map_err(|_| LogError::AlreadyInitialized)?;
    let _ = LEVEL_HANDLE.set(handle);
    *CURRENT_LEVEL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(config.level);
    Ok(LogGuard { sink })
}

/// Network libraries log peer addresses and request targets at `debug` and `trace`. Those are
/// upstream server addresses, which the project keeps out of logs, so these targets never log
/// below `warn` whatever the app level is.
const NETWORK_TARGETS: &[&str] = &[
    "hyper",
    "hyper_util",
    "reqwest",
    "h2",
    "rustls",
    "tokio_rustls",
    "tungstenite",
    "tokio_tungstenite",
    "tower",
    "tower_http",
    "mio",
];

fn targets(level: LogLevel) -> Targets {
    let app = level.filter();
    let capped = app.min(LevelFilter::WARN);
    let mut targets = Targets::new().with_default(app);
    for name in NETWORK_TARGETS {
        targets = targets.with_target(*name, capped);
    }
    targets
}

/// Logs every panic (message and location, redacted like all lines) before the previous hook
/// runs, so the usual panic output still appears.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|at| format!("{}:{}", at.file(), at.line()))
            .unwrap_or_default();
        tracing::error!(target: "panic", %location, "{}", payload_text(info.payload()));
        previous(info);
    }));
}

fn payload_text(payload: &(dyn Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        return (*text).to_owned();
    }
    if let Some(text) = payload.downcast_ref::<String>() {
        return text.clone();
    }
    "non-text panic payload".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_names_parse_case_insensitively() {
        for (text, level) in [
            ("off", LogLevel::Off),
            ("ERROR", LogLevel::Error),
            (" Warn ", LogLevel::Warn),
            ("warning", LogLevel::Warn),
            ("info", LogLevel::Info),
            ("Debug", LogLevel::Debug),
            ("TRACE", LogLevel::Trace),
        ] {
            assert_eq!(text.parse(), Ok(level), "{text}");
        }
        assert!("verbose".parse::<LogLevel>().is_err());
        assert!("".parse::<LogLevel>().is_err());
    }

    #[test]
    fn environment_beats_setting_and_bad_values_fall_through() {
        assert_eq!(resolve_level(Some("debug"), Some("warn")), LogLevel::Warn);
        assert_eq!(resolve_level(Some("debug"), None), LogLevel::Debug);
        assert_eq!(resolve_level(Some("debug"), Some("loud")), LogLevel::Debug);
        assert_eq!(resolve_level(Some("loud"), Some("loud")), LogLevel::Info);
        assert_eq!(resolve_level(None, None), LogLevel::Info);
    }

    #[test]
    fn crash_folder_is_inside_the_log_folder() {
        if let (Some(logs), Some(crashes)) = (log_directory(), crash_directory()) {
            assert_eq!(crashes.parent(), Some(logs.as_path()));
            assert!(logs.ends_with("logs"));
        }
    }

    #[test]
    fn panic_payloads_read_str_and_string_only() {
        assert_eq!(payload_text(&"plain"), "plain");
        assert_eq!(payload_text(&String::from("owned")), "owned");
        assert_eq!(payload_text(&42_u32), "non-text panic payload");
    }
}

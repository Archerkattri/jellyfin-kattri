//! Refresh-rate matching (M6-04): pick the display mode that suits the content's frame rate, and
//! remember the original mode so it is always put back. Pure: the display itself sits behind
//! [`DisplayBackend`], which the platform code implements and the tests fake.
//!
//! The rule, in one place:
//! - Only modes with the current resolution are considered. Switching resolution is never done.
//! - A mode matches when its refresh rate is within 0.2 % of a whole multiple (1 to 5) of the
//!   content rate. That accepts 23.976 on 24 Hz and 59.94 on 60 Hz, and rejects 25 fps on 24 Hz.
//! - The smallest multiple wins, because it changes the picture cadence least; with
//!   `avoid_25_30_hz` the content is never shown at 1x if that would be 25 or 30 Hz, so 25 fps
//!   picks 50 Hz and 30 fps picks 60 Hz (many panels judder at 25 and 30).
//! - Among modes with the same multiple, the smaller error wins (24.000 for 24 fps, not 23.976
//!   when both are offered and the content is exactly 24).
//! - If the current mode already matches, nothing changes.

use std::fmt;

/// Tolerance of a match, as a fraction of the target rate.
pub const MATCH_TOLERANCE: f64 = 0.002;
/// The largest multiple of the content rate that is tried.
pub const MAX_MULTIPLE: u32 = 5;

/// One display mode. The refresh rate is in millihertz so 23.976 Hz stays exact.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DisplayMode {
    pub width: u32,
    pub height: u32,
    pub refresh_millihz: u32,
}

impl DisplayMode {
    pub const fn new(width: u32, height: u32, refresh_millihz: u32) -> Self {
        Self {
            width,
            height,
            refresh_millihz,
        }
    }

    pub fn hz(self) -> f64 {
        f64::from(self.refresh_millihz) / 1000.0
    }
}

impl fmt::Display for DisplayMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x{} @ {:.3} Hz", self.width, self.height, self.hz())
    }
}

/// The multiple of `content_fps` that `mode` runs at, if it is within tolerance of one.
pub fn multiple_of(mode: DisplayMode, content_fps: f64) -> Option<(u32, f64)> {
    if !content_fps.is_finite() || content_fps <= 0.0 {
        return None;
    }
    (1..=MAX_MULTIPLE).find_map(|multiple| {
        let target = content_fps * f64::from(multiple);
        let error = (mode.hz() - target).abs() / target;
        (error <= MATCH_TOLERANCE).then_some((multiple, error))
    })
}

/// The mode to switch to, or `None` when the current mode already suits the content or no
/// offered mode does (content rate unknown, or no matching mode at this resolution).
pub fn pick_mode(
    content_fps: f64,
    current: DisplayMode,
    available: &[DisplayMode],
    avoid_25_30_hz: bool,
) -> Option<DisplayMode> {
    if !content_fps.is_finite() || content_fps <= 0.0 {
        return None;
    }
    let first_multiple = |mode: DisplayMode| -> Option<(u32, f64)> {
        let (multiple, error) = multiple_of(mode, content_fps)?;
        // 25 and 30 Hz are only acceptable when the viewer did not ask to avoid them.
        let judders = multiple == 1 && avoid_25_30_hz && is_25_or_30(mode.hz());
        (!judders).then_some((multiple, error))
    };
    if first_multiple(current).is_some() {
        return None;
    }
    available
        .iter()
        .copied()
        .filter(|mode| mode.width == current.width && mode.height == current.height)
        .filter_map(|mode| first_multiple(mode).map(|(multiple, error)| (mode, multiple, error)))
        .min_by(|a, b| {
            (a.1, a.2.to_bits(), a.0.refresh_millihz).cmp(&(
                b.1,
                b.2.to_bits(),
                b.0.refresh_millihz,
            ))
        })
        .map(|(mode, _, _)| mode)
}

fn is_25_or_30(hz: f64) -> bool {
    [25.0, 30.0, 24.975, 29.97]
        .iter()
        .any(|rate| (hz - rate).abs() / rate <= MATCH_TOLERANCE)
}

/// The display, as the platform code sees it.
pub trait DisplayBackend {
    type Error: fmt::Display;
    fn current_mode(&mut self) -> Result<DisplayMode, Self::Error>;
    fn available_modes(&mut self) -> Result<Vec<DisplayMode>, Self::Error>;
    fn set_mode(&mut self, mode: DisplayMode) -> Result<(), Self::Error>;
}

/// Where the original mode is written while a switch is active, so a crash cannot leave the
/// display on the wrong mode: the next launch calls [`recover`].
pub trait RecordStore {
    type Error: fmt::Display;
    fn save(&mut self, original: DisplayMode) -> Result<(), Self::Error>;
    fn load(&mut self) -> Result<Option<DisplayMode>, Self::Error>;
    fn clear(&mut self) -> Result<(), Self::Error>;
}

/// What a call did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The display was switched to this mode.
    Switched(DisplayMode),
    /// The current mode already suits the content, or nothing suits it: nothing changed.
    Unchanged,
    /// The original mode was restored.
    Restored(DisplayMode),
    /// There was nothing to restore.
    NothingToRestore,
}

/// Why a call failed. The text names modes and rates, never paths or credentials.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MatchError {
    Display(String),
    Record(String),
    /// The switch happened but the original mode could not be written first, so it was undone.
    /// Nothing is left changed.
    RecordFailedSwitchUndone(String),
    /// The original mode could not be put back. The message tells the viewer which mode to set
    /// by hand.
    RestoreFailed {
        original: DisplayMode,
        reason: String,
    },
}

impl fmt::Display for MatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Display(reason) => write!(f, "the display refused the change: {reason}"),
            Self::Record(reason) => write!(f, "could not save the original display mode: {reason}"),
            Self::RecordFailedSwitchUndone(reason) => write!(
                f,
                "the display mode was left alone because the original could not be saved: {reason}"
            ),
            Self::RestoreFailed { original, reason } => write!(
                f,
                "could not restore the display mode ({reason}); set it back to {original} in your display settings"
            ),
        }
    }
}

impl std::error::Error for MatchError {}

/// A switch in progress. Holds the mode to go back to.
pub struct Matcher<B, S> {
    backend: B,
    store: S,
    original: Option<DisplayMode>,
}

impl<B: DisplayBackend, S: RecordStore> Matcher<B, S> {
    pub fn new(backend: B, store: S) -> Self {
        Self {
            backend,
            store,
            original: None,
        }
    }

    /// Put the display back after a crash: if a record of an original mode is stored, restore it
    /// and clear the record. Call once at start-up, before any playback.
    pub fn recover(&mut self) -> Result<Outcome, MatchError> {
        let Some(original) = self
            .store
            .load()
            .map_err(|e| MatchError::Record(e.to_string()))?
        else {
            return Ok(Outcome::NothingToRestore);
        };
        self.restore_to(original)
    }

    /// Switch to the mode that suits `content_fps`. The original mode is saved before the switch
    /// and kept across later calls (changing from 24 to 50 Hz still restores the first mode).
    pub fn apply(&mut self, content_fps: f64, avoid_25_30_hz: bool) -> Result<Outcome, MatchError> {
        let current = self
            .backend
            .current_mode()
            .map_err(|e| MatchError::Display(e.to_string()))?;
        let available = self
            .backend
            .available_modes()
            .map_err(|e| MatchError::Display(e.to_string()))?;
        // Measure from the display's own mode when nothing is switched, else from the mode the
        // user had before the first switch, so the resolution check stays on their resolution.
        let base = self.original.unwrap_or(current);
        let Some(target) = pick_mode(content_fps, current, &available, avoid_25_30_hz)
            .filter(|mode| (mode.width, mode.height) == (base.width, base.height))
        else {
            return Ok(Outcome::Unchanged);
        };
        let first_switch = self.original.is_none();
        if first_switch {
            self.store
                .save(current)
                .map_err(|e| MatchError::RecordFailedSwitchUndone(e.to_string()))?;
        }
        if let Err(error) = self.backend.set_mode(target) {
            if first_switch {
                let _ = self.store.clear();
            }
            return Err(MatchError::Display(error.to_string()));
        }
        if first_switch {
            self.original = Some(current);
        }
        Ok(Outcome::Switched(target))
    }

    /// Put the original mode back (on stop, on exit). A failure keeps the record so the next
    /// launch tries again.
    pub fn restore(&mut self) -> Result<Outcome, MatchError> {
        let Some(original) = self.original else {
            return Ok(Outcome::NothingToRestore);
        };
        self.restore_to(original)
    }

    fn restore_to(&mut self, original: DisplayMode) -> Result<Outcome, MatchError> {
        self.backend
            .set_mode(original)
            .map_err(|e| MatchError::RestoreFailed {
                original,
                reason: e.to_string(),
            })?;
        self.original = None;
        self.store
            .clear()
            .map_err(|e| MatchError::Record(e.to_string()))?;
        Ok(Outcome::Restored(original))
    }

    pub fn is_switched(&self) -> bool {
        self.original.is_some()
    }

    pub fn into_parts(self) -> (B, S) {
        (self.backend, self.store)
    }
}

/// The record as one line of text, `<width>x<height>@<millihertz>`.
pub fn encode_record(mode: DisplayMode) -> String {
    format!("{}x{}@{}", mode.width, mode.height, mode.refresh_millihz)
}

/// Read a record written by [`encode_record`]. Anything else is `None`: a damaged record must
/// not drive the display.
pub fn decode_record(text: &str) -> Option<DisplayMode> {
    let (size, rate) = text.trim().split_once('@')?;
    let (width, height) = size.split_once('x')?;
    let mode = DisplayMode::new(
        width.parse().ok()?,
        height.parse().ok()?,
        rate.parse().ok()?,
    );
    let sane = (1..=16_384).contains(&mode.width)
        && (1..=16_384).contains(&mode.height)
        && (1_000..=1_000_000).contains(&mode.refresh_millihz);
    sane.then_some(mode)
}

/// The original-mode record in one small file. Writes go through a temporary file and a rename,
/// so a crash leaves the old record or the new one, never half of one.
pub struct FileRecordStore {
    path: std::path::PathBuf,
}

impl FileRecordStore {
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl RecordStore for FileRecordStore {
    type Error = std::io::Error;

    fn save(&mut self, original: DisplayMode) -> Result<(), Self::Error> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temp = self.path.with_extension("tmp");
        std::fs::write(&temp, encode_record(original))?;
        std::fs::rename(&temp, &self.path).inspect_err(|_| {
            let _ = std::fs::remove_file(&temp);
        })
    }

    fn load(&mut self) -> Result<Option<DisplayMode>, Self::Error> {
        match std::fs::read_to_string(&self.path) {
            // A damaged record is dropped rather than obeyed.
            Ok(text) => Ok(decode_record(&text)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn clear(&mut self) -> Result<(), Self::Error> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    const fn m(hz_milli: u32) -> DisplayMode {
        DisplayMode::new(1920, 1080, hz_milli)
    }

    fn standard_modes() -> Vec<DisplayMode> {
        vec![
            m(23_976),
            m(24_000),
            m(25_000),
            m(29_970),
            m(30_000),
            m(50_000),
            m(59_940),
            m(60_000),
            m(120_000),
            DisplayMode::new(3840, 2160, 24_000),
            DisplayMode::new(3840, 2160, 60_000),
        ]
    }

    fn pick(fps: f64, current_milli: u32, avoid: bool) -> Option<u32> {
        pick_mode(fps, m(current_milli), &standard_modes(), avoid).map(|mode| mode.refresh_millihz)
    }

    #[test]
    fn common_content_rates_pick_the_matching_mode_from_a_60_hz_desktop() {
        // (content fps, expected refresh in mHz)
        for (fps, expected) in [
            (23.976, 23_976),
            (24.0, 24_000),
            (25.0, 50_000),
            (50.0, 50_000),
        ] {
            assert_eq!(pick(fps, 60_000, true), Some(expected), "{fps} fps");
        }
    }

    #[test]
    fn a_current_mode_that_already_suits_the_content_is_left_alone() {
        assert_eq!(pick(60.0, 60_000, true), None);
        assert_eq!(pick(30.0, 60_000, true), None);
        assert_eq!(pick(24.0, 24_000, true), None);
        assert_eq!(pick(23.976, 23_976, true), None);
        assert_eq!(pick(29.97, 59_940, true), None);
        // 60.000 Hz is within the tolerance of 59.94 fps, and of 29.97 fps doubled: no switch.
        assert_eq!(pick(59.94, 60_000, true), None);
        assert_eq!(pick(29.97, 60_000, true), None);
    }

    #[test]
    fn avoiding_25_and_30_hz_doubles_those_rates_but_keeping_them_is_allowed() {
        assert_eq!(pick(25.0, 60_000, true), Some(50_000));
        assert_eq!(pick(25.0, 60_000, false), Some(25_000));
        assert_eq!(pick(30.0, 24_000, true), Some(60_000));
        assert_eq!(pick(30.0, 24_000, false), Some(30_000));
        // Already at 25 Hz with the avoid setting on: move off it.
        assert_eq!(pick(25.0, 25_000, true), Some(50_000));
    }

    #[test]
    fn the_exact_mode_beats_a_near_one_at_the_same_multiple() {
        assert_eq!(pick(24.0, 60_000, true), Some(24_000));
        assert_eq!(pick(23.976, 60_000, true), Some(23_976));
    }

    #[test]
    fn rates_that_fit_no_multiple_change_nothing() {
        // 15 fps has multiples 15, 30, 45, 60, 75: 30 and 60 exist.
        assert_eq!(pick(15.0, 24_000, false), Some(30_000));
        // 17 fps: 17, 34, 51, 68, 85 offer nothing.
        assert_eq!(pick(17.0, 60_000, false), None);
    }

    #[test]
    fn an_unknown_content_rate_never_changes_the_display() {
        for fps in [0.0, -24.0, f64::NAN, f64::INFINITY] {
            assert_eq!(pick(fps, 60_000, true), None, "{fps}");
        }
    }

    #[test]
    fn the_resolution_is_never_changed() {
        let only_4k = [DisplayMode::new(3840, 2160, 24_000)];
        assert_eq!(pick_mode(24.0, m(60_000), &only_4k, true), None);
    }

    #[test]
    fn multiples_are_matched_within_the_tolerance() {
        let (multiple, error) = multiple_of(m(24_000), 23.976).expect("24 Hz suits 23.976 fps");
        assert_eq!(multiple, 1);
        assert!(error < 0.0011, "{error}");
        assert!(multiple_of(m(25_000), 24.0).is_none());
        assert_eq!(multiple_of(m(120_000), 24.0).map(|(k, _)| k), Some(5));
        assert!(multiple_of(m(60_000), 0.0).is_none());
    }

    // -- the display, faked ---------------------------------------------------------------

    #[derive(Clone)]
    struct FakeDisplay {
        state: Rc<RefCell<FakeState>>,
    }

    struct FakeState {
        mode: DisplayMode,
        modes: Vec<DisplayMode>,
        history: Vec<DisplayMode>,
        fail_set_for: Vec<DisplayMode>,
    }

    impl FakeDisplay {
        fn new(mode: DisplayMode) -> Self {
            Self {
                state: Rc::new(RefCell::new(FakeState {
                    mode,
                    modes: standard_modes(),
                    history: Vec::new(),
                    fail_set_for: Vec::new(),
                })),
            }
        }
        fn mode(&self) -> DisplayMode {
            self.state.borrow().mode
        }
    }

    impl DisplayBackend for FakeDisplay {
        type Error = String;
        fn current_mode(&mut self) -> Result<DisplayMode, String> {
            Ok(self.state.borrow().mode)
        }
        fn available_modes(&mut self) -> Result<Vec<DisplayMode>, String> {
            Ok(self.state.borrow().modes.clone())
        }
        fn set_mode(&mut self, mode: DisplayMode) -> Result<(), String> {
            let mut state = self.state.borrow_mut();
            if state.fail_set_for.contains(&mode) {
                return Err(format!("{mode} is not supported"));
            }
            state.mode = mode;
            state.history.push(mode);
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    struct FakeRecord {
        stored: Rc<RefCell<Option<DisplayMode>>>,
        fail_save: Rc<RefCell<bool>>,
    }

    impl RecordStore for FakeRecord {
        type Error = String;
        fn save(&mut self, original: DisplayMode) -> Result<(), String> {
            if *self.fail_save.borrow() {
                return Err("disk full".to_owned());
            }
            *self.stored.borrow_mut() = Some(original);
            Ok(())
        }
        fn load(&mut self) -> Result<Option<DisplayMode>, String> {
            Ok(*self.stored.borrow())
        }
        fn clear(&mut self) -> Result<(), String> {
            *self.stored.borrow_mut() = None;
            Ok(())
        }
    }

    fn rig() -> (FakeDisplay, FakeRecord, Matcher<FakeDisplay, FakeRecord>) {
        let display = FakeDisplay::new(m(60_000));
        let record = FakeRecord::default();
        let matcher = Matcher::new(display.clone(), record.clone());
        (display, record, matcher)
    }

    #[test]
    fn a_24_fps_file_switches_to_24_hz_and_stop_restores_60_hz() {
        let (display, record, mut matcher) = rig();
        assert_eq!(matcher.apply(24.0, true), Ok(Outcome::Switched(m(24_000))));
        assert_eq!(display.mode(), m(24_000));
        assert_eq!(*record.stored.borrow(), Some(m(60_000)));
        assert!(matcher.is_switched());

        assert_eq!(matcher.restore(), Ok(Outcome::Restored(m(60_000))));
        assert_eq!(display.mode(), m(60_000));
        assert_eq!(*record.stored.borrow(), None);
        assert!(!matcher.is_switched());
        assert_eq!(matcher.restore(), Ok(Outcome::NothingToRestore));
    }

    #[test]
    fn switching_again_keeps_the_first_original_mode() {
        let (display, record, mut matcher) = rig();
        matcher.apply(24.0, true).unwrap();
        assert_eq!(matcher.apply(25.0, true), Ok(Outcome::Switched(m(50_000))));
        assert_eq!(*record.stored.borrow(), Some(m(60_000)));
        matcher.restore().unwrap();
        assert_eq!(display.mode(), m(60_000));
    }

    #[test]
    fn content_that_already_fits_does_not_touch_the_display_or_the_record() {
        let (display, record, mut matcher) = rig();
        assert_eq!(matcher.apply(60.0, true), Ok(Outcome::Unchanged));
        assert!(display.state.borrow().history.is_empty());
        assert_eq!(*record.stored.borrow(), None);
    }

    #[test]
    fn a_refused_switch_leaves_no_record_and_reports_the_reason() {
        let (display, record, mut matcher) = rig();
        display.state.borrow_mut().fail_set_for = vec![m(24_000)];
        let error = matcher.apply(24.0, true).unwrap_err();
        assert!(matches!(error, MatchError::Display(_)), "{error}");
        assert_eq!(display.mode(), m(60_000));
        assert_eq!(*record.stored.borrow(), None);
        assert!(!matcher.is_switched());
    }

    #[test]
    fn an_unwritable_record_stops_the_switch_before_it_happens() {
        let (display, record, mut matcher) = rig();
        *record.fail_save.borrow_mut() = true;
        let error = matcher.apply(24.0, true).unwrap_err();
        assert!(
            matches!(error, MatchError::RecordFailedSwitchUndone(_)),
            "{error}"
        );
        assert_eq!(display.mode(), m(60_000));
        assert!(display.state.borrow().history.is_empty());
    }

    #[test]
    fn a_failed_restore_names_the_mode_to_set_by_hand_and_keeps_the_record() {
        let (display, record, mut matcher) = rig();
        matcher.apply(24.0, true).unwrap();
        display.state.borrow_mut().fail_set_for = vec![m(60_000)];
        let error = matcher.restore().unwrap_err();
        let MatchError::RestoreFailed { original, .. } = &error else {
            panic!("{error}");
        };
        assert_eq!(*original, m(60_000));
        assert!(
            error.to_string().contains("1920x1080 @ 60.000 Hz"),
            "{error}"
        );
        assert_eq!(*record.stored.borrow(), Some(m(60_000)));
        assert!(matcher.is_switched());
    }

    #[test]
    fn the_next_launch_restores_the_mode_a_crash_left_behind() {
        let (display, record, mut matcher) = rig();
        matcher.apply(24.0, true).unwrap();
        // The process dies here: the matcher is gone, the display stays on 24 Hz, and the record
        // on disk still names 60 Hz.
        drop(matcher);
        assert_eq!(display.mode(), m(24_000));

        let mut next_launch = Matcher::new(display.clone(), record.clone());
        assert_eq!(next_launch.recover(), Ok(Outcome::Restored(m(60_000))));
        assert_eq!(display.mode(), m(60_000));
        assert_eq!(*record.stored.borrow(), None);
        assert_eq!(next_launch.recover(), Ok(Outcome::NothingToRestore));
    }

    #[test]
    fn records_round_trip_and_damaged_records_are_ignored() {
        for mode in [m(23_976), DisplayMode::new(3840, 2160, 119_880)] {
            assert_eq!(decode_record(&encode_record(mode)), Some(mode));
        }
        assert_eq!(decode_record("1920x1080@60000\n"), Some(m(60_000)));
        for bad in [
            "",
            "garbage",
            "1920x1080",
            "0x1080@60000",
            "1920x0@60000",
            "1920x1080@0",
            "99999x1080@60000",
            "1920x1080@999999999",
            "1920 x 1080 @ 60000",
            "-1x1080@60000",
        ] {
            assert_eq!(decode_record(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_file_store_saves_loads_and_clears_a_record() {
        let dir = std::env::temp_dir().join(format!("kattri-display-{}", std::process::id()));
        let path = dir.join("nested").join("display-mode.txt");
        let _ = std::fs::remove_dir_all(&dir);
        let mut store = FileRecordStore::new(&path);
        assert_eq!(store.load().unwrap(), None);
        store.save(m(60_000)).unwrap();
        assert_eq!(store.load().unwrap(), Some(m(60_000)));
        assert!(!path.with_extension("tmp").exists());
        store.save(m(59_940)).unwrap();
        assert_eq!(store.load().unwrap(), Some(m(59_940)));
        store.clear().unwrap();
        store.clear().unwrap();
        assert_eq!(store.load().unwrap(), None);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "not a mode").unwrap();
        assert_eq!(store.load().unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

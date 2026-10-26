//! Per-profile keyboard and controller maps (M2-17), as a pure model.
//!
//! File format (`<profile>/inputmaps/<device>.json`):
//!
//! ```text
//! { "schemaVersion": 1, "bindings": { "play_pause": ["key:P", "key:Space"] } }
//! ```
//!
//! - Action names are the legacy Neo names (see [`legacy_name`]). Names this build does
//!   not know are kept and ignored at runtime.
//! - Binding strings: `key:<Mods+Key>` for keyboard keys (modifiers `Ctrl`, `Shift`,
//!   `Alt`, `Meta`), and `pad:<Name>` for controller buttons. Unknown strings are kept.
//!   [UNCONFIRMED] the `pad:` name set; the spec names `pad:South`, `pad:DPadUp` as
//!   examples. This module accepts any non-empty name without whitespace and does not
//!   map it to a device button. That mapping belongs to the gamepad backend, which is
//!   an owner decision (see README).
//! - Bindings are stored in canonical form (modifier order `Ctrl+Shift+Alt+Meta`), so
//!   `key:Shift+Ctrl+P` and `key:Ctrl+Shift+P` are the same binding.
//!
//! Rules:
//! - Conflicts are reported, never resolved here: [`InputMap::conflicts`] lists each
//!   binding used by more than one action. The UI decides whether to move or keep both.
//! - User edits accept at most [`MAX_USER_BINDINGS`] bindings per action, and the
//!   essential actions in [`ESSENTIAL_ACTIONS`] cannot lose their last binding.
//!   Loading a file does not enforce these caps, because the bundled legacy defaults
//!   already exceed three bindings for some actions.
//!
//! Reading and writing files is in [`crate::input_store`]; the rebind flow is in
//! [`crate::rebind`]. Not implemented: the folder watcher and the egui Input page.
//! [`InputMap::reload`] gives the "keep last good map" rule as a pure function.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::action::{ALL_ACTIONS, Action, HostAction};
use crate::json_keys::duplicate_object_keys;
use crate::keymap::KeyChord;

/// The only file schema version this build reads and writes.
pub const SCHEMA_VERSION: u64 = 1;

/// Maximum bindings a user can set for one action through [`InputMap::add_binding`]
/// and [`InputMap::set_bindings`].
pub const MAX_USER_BINDINGS: usize = 3;

/// Actions that must keep at least one binding, so the user cannot lock themselves out.
/// The spec names `Back` as the example. [UNCONFIRMED] whether `Enter` belongs here too.
pub const ESSENTIAL_ACTIONS: &[Action] = &[Action::Back];

/// Keyboard defaults. Most come from the legacy Neo keymap
/// (`legacy/qt-neo/resources/inputmaps/keyboard.json`), restricted to the actions in
/// [`Action`]. Long-press (`short`/`long`) behaviour is flattened to the short action,
/// so `Esc` and `Backspace` are bound to `back` only. Entries the legacy map does not
/// have are spec-only defaults (see the tests for the exact list): they were chosen so
/// they do not collide with any other default in this table.
const LEGACY_KEYBOARD_DEFAULTS: &[(Action, &[&str])] = &[
    // WASD navigates. `A` and `S` were cycle_audio and toggle_subtitles in the legacy map;
    // they moved to `E` and `C`, and `M` (mute) moved to `N`, so that each default key carries
    // one action (see keyboard_reachability.rs). `M` now opens Settings.
    (Action::Up, &["key:Up", "key:Shift+Up", "key:W"]),
    (Action::Down, &["key:Down", "key:Shift+Down", "key:S"]),
    (Action::Left, &["key:Left", "key:Shift+Left", "key:A"]),
    (Action::Right, &["key:Right", "key:Shift+Right", "key:D"]),
    (Action::Enter, &["key:Return", "key:Enter"]),
    (
        Action::Back,
        &["key:Esc", "key:Backspace", "key:B", "key:Back"],
    ),
    (Action::Home, &["key:H"]),
    (Action::Search, &["key:Ctrl+F"]),
    (Action::Settings, &["key:M"]),
    (
        Action::PlayPause,
        &[
            "key:P",
            "key:Space",
            "key:Ctrl+Shift+P",
            "key:Media Play",
            "key:Toggle Media Play/Pause",
        ],
    ),
    (Action::Pause, &["key:Ctrl+P", "key:Media Pause"]),
    (
        Action::Stop,
        &["key:X", "key:Ctrl+Shift+S", "key:Media Stop"],
    ),
    (
        Action::SeekBackward,
        &["key:PgUp", "key:Ctrl+Shift+B", "key:Media Rewind"],
    ),
    (
        Action::SeekForward,
        &["key:PgDown", "key:Ctrl+Shift+G", "key:Media Fast Forward"],
    ),
    (
        Action::StepBackward,
        &["key:Home", "key:Ctrl+B", "key:Media Previous"],
    ),
    (
        Action::StepForward,
        &["key:End", "key:Ctrl+G", "key:Media Next"],
    ),
    (Action::CycleAudio, &["key:E"]),
    (Action::CycleAudioBack, &["key:Ctrl+A"]),
    (Action::CycleSubtitles, &["key:L"]),
    (Action::CycleSubtitlesBack, &["key:Ctrl+L"]),
    (Action::ToggleSubtitles, &["key:C"]),
    (Action::IncreaseAudioDelay, &["key:Alt+A"]),
    (Action::DecreaseAudioDelay, &["key:Alt+Shift+A"]),
    // `]` and `[` nudge the subtitle delay later and earlier (spec M2 subtitle sync).
    (Action::IncreaseSubtitleDelay, &["key:Alt+S", "key:]"]),
    (Action::DecreaseSubtitleDelay, &["key:Alt+Shift+S", "key:["]),
    (Action::ResetSubtitleDelay, &["key:Alt+R"]),
    // `=` and `-` are the legacy volume keys. `+` is not a valid chord yet (see README).
    (Action::IncreaseVolume, &["key:="]),
    (Action::DecreaseVolume, &["key:-"]),
    (Action::Mute, &["key:N"]),
    (Action::SpeedUp, &["key:Ctrl+Up"]),
    (Action::SpeedDown, &["key:Ctrl+Down"]),
    (Action::ChapterNext, &["key:Ctrl+PgDown"]),
    (Action::ChapterPrevious, &["key:Ctrl+PgUp"]),
    // Esc's long press is `exit` in the legacy map. The flat format has no long press,
    // so `exit` gets its own key here.
    (Action::Exit, &["key:Shift+Esc"]),
    (Action::Host(HostAction::Screenshot), &["key:Ctrl+S"]),
    (
        Action::Host(HostAction::Minimize),
        &["key:Ctrl+M", "key:Meta+Down"],
    ),
    (
        Action::Host(HostAction::Fullscreen),
        &[
            "key:F11",
            "key:Shift+F11",
            "key:Meta+Ctrl+F",
            "key:Alt+Return",
            "key:Alt+Enter",
            "key:\\",
        ],
    ),
    (
        Action::Host(HostAction::ToggleWebMode),
        &["key:Ctrl+Shift+F", "key:Meta+Enter"],
    ),
    (Action::Host(HostAction::Quit), &["key:Ctrl+W"]),
    (Action::Host(HostAction::Close), &["key:Ctrl+Q"]),
    (
        Action::Host(HostAction::ToggleDebug),
        &["key:Ctrl+Shift+D", "key:I"],
    ),
    // Unbound by default. `Ctrl+Shift+P` is already a legacy alias of play/pause, so the
    // PiP default is an owner decision (M2-17 open question). Bind it in the user's map.
    (Action::Host(HostAction::TogglePip), &[]),
];

/// The file name used for an action (legacy spelling). Exhaustive, so a new [`Action`]
/// variant fails to compile until it gets a name.
pub fn legacy_name(action: Action) -> &'static str {
    match action {
        Action::Up => "up",
        Action::Down => "down",
        Action::Left => "left",
        Action::Right => "right",
        Action::Enter => "enter",
        Action::Back => "back",
        Action::Exit => "exit",
        Action::Home => "home",
        Action::Search => "search",
        Action::Settings => "settings",
        Action::PlayPause => "play_pause",
        Action::Pause => "pause",
        Action::Stop => "stop",
        Action::SeekBackward => "seek_backward",
        Action::SeekForward => "seek_forward",
        Action::StepBackward => "step_backward",
        Action::StepForward => "step_forward",
        Action::CycleAudio => "cycle_audio",
        Action::CycleAudioBack => "cycle_audio_back",
        Action::CycleSubtitles => "cycle_subtitles",
        Action::CycleSubtitlesBack => "cycle_subtitles_back",
        Action::ToggleSubtitles => "toggle_subtitles",
        Action::IncreaseAudioDelay => "increase_audio_delay",
        Action::DecreaseAudioDelay => "decrease_audio_delay",
        Action::IncreaseSubtitleDelay => "increase_subtitles_delay",
        Action::DecreaseSubtitleDelay => "decrease_subtitles_delay",
        Action::ResetSubtitleDelay => "reset_subtitles_delay",
        Action::IncreaseVolume => "increase_volume",
        Action::DecreaseVolume => "decrease_volume",
        Action::Mute => "mute",
        Action::SpeedUp => "speed_up",
        Action::SpeedDown => "speed_down",
        Action::ChapterNext => "chapter_next",
        Action::ChapterPrevious => "chapter_previous",
        Action::Host(HostAction::Screenshot) => "host:screenshot",
        Action::Host(HostAction::Minimize) => "host:minimize",
        Action::Host(HostAction::Fullscreen) => "host:fullscreen",
        Action::Host(HostAction::ToggleWebMode) => "host:toggleWebMode",
        Action::Host(HostAction::Quit) => "host:quit",
        Action::Host(HostAction::Close) => "host:close",
        Action::Host(HostAction::ToggleDebug) => "host:toggleDebug",
        Action::Host(HostAction::TogglePip) => "host:togglePip",
    }
}

/// A parsed binding string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingSpec {
    Key(KeyChord),
    /// Controller button name, as written after `pad:`.
    Pad(String),
    /// Anything else. Kept in files, ignored at runtime, and rejected for user edits.
    Unknown(String),
}

impl BindingSpec {
    /// Parse `key:...` or `pad:...`. Anything else, or a malformed key chord, is `Unknown`.
    pub fn parse(text: &str) -> Self {
        if let Some(rest) = text.strip_prefix("key:") {
            match KeyChord::parse(rest) {
                Ok(chord) => Self::Key(chord),
                Err(_) => Self::Unknown(text.to_owned()),
            }
        } else if let Some(rest) = text.strip_prefix("pad:") {
            if !rest.is_empty() && !rest.chars().any(char::is_whitespace) {
                Self::Pad(rest.to_owned())
            } else {
                Self::Unknown(text.to_owned())
            }
        } else {
            Self::Unknown(text.to_owned())
        }
    }

    /// The canonical string for this binding. `Unknown` is returned unchanged.
    pub fn canonical(&self) -> String {
        match self {
            Self::Key(chord) => {
                let mut out = String::from("key:");
                if chord.ctrl {
                    out.push_str("Ctrl+");
                }
                if chord.shift {
                    out.push_str("Shift+");
                }
                if chord.alt {
                    out.push_str("Alt+");
                }
                if chord.meta {
                    out.push_str("Meta+");
                }
                out.push_str(&chord.key);
                out
            }
            Self::Pad(name) => format!("pad:{name}"),
            Self::Unknown(raw) => raw.clone(),
        }
    }
}

/// Canonical form of a binding string (see [`BindingSpec::canonical`]).
pub fn normalize_binding(text: &str) -> String {
    BindingSpec::parse(text).canonical()
}

/// One binding used by more than one action in the same map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// Canonical binding string.
    pub binding: String,
    /// Every action bound to it, in file-name order.
    pub actions: Vec<Action>,
}

/// Errors from reading or editing a map.
#[derive(Debug, thiserror::Error)]
pub enum InputMapError {
    #[error("input map is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("input map `schemaVersion` must be a non-negative integer")]
    SchemaVersion,
    #[error("input map schemaVersion {0} is not supported (this build reads 1)")]
    UnsupportedSchemaVersion(u64),
    #[error("input map `bindings` must be an object of string arrays")]
    Bindings,
    #[error("`{0}` is not a valid binding (use key:... or pad:...)")]
    InvalidBinding(String),
    #[error("at most {MAX_USER_BINDINGS} bindings per action")]
    TooManyBindings,
    #[error("this binding is already set for the action")]
    DuplicateBinding,
    #[error("Keep at least one binding for {0}")]
    EssentialBinding(String),
}

/// A map read from a file, with notices that do not stop the load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    pub map: InputMap,
    pub warnings: Vec<String>,
}

/// The bindings for one profile and device family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputMap {
    /// Keyed by legacy action name, so unknown names survive a round trip.
    bindings: BTreeMap<String, Vec<String>>,
}

impl Default for InputMap {
    fn default() -> Self {
        Self::legacy_defaults()
    }
}

impl InputMap {
    /// A map with no bindings at all.
    pub fn empty() -> Self {
        Self {
            bindings: BTreeMap::new(),
        }
    }

    /// The keyboard defaults that match the legacy Neo keymap (see the constant above).
    pub fn legacy_defaults() -> Self {
        let mut map = Self::empty();
        for (action, specs) in LEGACY_KEYBOARD_DEFAULTS {
            let list = specs.iter().map(|s| normalize_binding(s)).collect();
            map.bindings.insert(legacy_name(*action).to_owned(), list);
        }
        map
    }

    /// Bindings for an action, in canonical form. Empty if unbound.
    pub fn bindings(&self, action: Action) -> &[String] {
        match self.bindings.get(legacy_name(action)) {
            Some(list) => list.as_slice(),
            None => &[],
        }
    }

    /// Replace all bindings for an action after validating them.
    pub fn set_bindings(&mut self, action: Action, bindings: &[&str]) -> Result<(), InputMapError> {
        if bindings.len() > MAX_USER_BINDINGS {
            return Err(InputMapError::TooManyBindings);
        }
        let mut canon: Vec<String> = Vec::with_capacity(bindings.len());
        for text in bindings {
            let spec = BindingSpec::parse(text);
            if let BindingSpec::Unknown(raw) = &spec {
                return Err(InputMapError::InvalidBinding(raw.clone()));
            }
            let c = spec.canonical();
            if canon.contains(&c) {
                return Err(InputMapError::DuplicateBinding);
            }
            canon.push(c);
        }
        if canon.is_empty() && ESSENTIAL_ACTIONS.contains(&action) {
            return Err(InputMapError::EssentialBinding(action.to_string()));
        }
        self.bindings.insert(legacy_name(action).to_owned(), canon);
        Ok(())
    }

    /// Add one binding to an action. Conflicts with other actions are allowed here;
    /// check [`InputMap::also_bound_to`] first if the UI needs to ask.
    pub fn add_binding(&mut self, action: Action, binding: &str) -> Result<(), InputMapError> {
        let spec = BindingSpec::parse(binding);
        if let BindingSpec::Unknown(raw) = &spec {
            return Err(InputMapError::InvalidBinding(raw.clone()));
        }
        let c = spec.canonical();
        let key = legacy_name(action).to_owned();
        let list = self.bindings.entry(key).or_default();
        if list.contains(&c) {
            return Err(InputMapError::DuplicateBinding);
        }
        if list.len() >= MAX_USER_BINDINGS {
            return Err(InputMapError::TooManyBindings);
        }
        list.push(c);
        Ok(())
    }

    /// Replace one binding in place, keeping the action's other bindings and their order.
    /// If `old` is not set, the new binding is added instead. Replacing keeps the count, so
    /// the three-binding cap and the essential-action rule cannot be broken by it.
    pub fn replace_binding(
        &mut self,
        action: Action,
        old: &str,
        new: &str,
    ) -> Result<(), InputMapError> {
        let spec = BindingSpec::parse(new);
        if let BindingSpec::Unknown(raw) = &spec {
            return Err(InputMapError::InvalidBinding(raw.clone()));
        }
        let new_c = spec.canonical();
        let old_c = normalize_binding(old);
        let key = legacy_name(action).to_owned();
        let mut list = self.bindings.get(&key).cloned().unwrap_or_default();
        let Some(pos) = list.iter().position(|b| *b == old_c) else {
            return self.add_binding(action, new);
        };
        if list
            .iter()
            .enumerate()
            .any(|(i, b)| i != pos && *b == new_c)
        {
            return Err(InputMapError::DuplicateBinding);
        }
        list[pos] = new_c;
        self.bindings.insert(key, list);
        Ok(())
    }

    /// Remove one binding. Removing a binding that is not set is a no-op.
    pub fn remove_binding(&mut self, action: Action, binding: &str) -> Result<(), InputMapError> {
        let c = normalize_binding(binding);
        let Some(list) = self.bindings.get_mut(legacy_name(action)) else {
            return Ok(());
        };
        if !list.contains(&c) {
            return Ok(());
        }
        if list.len() == 1 && ESSENTIAL_ACTIONS.contains(&action) {
            return Err(InputMapError::EssentialBinding(action.to_string()));
        }
        list.retain(|b| b != &c);
        Ok(())
    }

    /// Other actions already bound to this binding. Empty when the binding is free
    /// or invalid.
    pub fn also_bound_to(&self, action: Action, binding: &str) -> Vec<Action> {
        let c = normalize_binding(binding);
        self.bindings
            .iter()
            .filter(|(name, list)| legacy_name_to_action(name) != Some(action) && list.contains(&c))
            .filter_map(|(name, _)| legacy_name_to_action(name))
            .collect()
    }

    /// Every action in [`ALL_ACTIONS`] that has no keyboard (`key:`) binding in this map.
    /// Controller bindings do not count. For the default map this should be only the
    /// actions the spec allows to be unbound (M2-18).
    pub fn actions_without_keyboard_binding(&self) -> Vec<Action> {
        ALL_ACTIONS
            .iter()
            .copied()
            .filter(|action| {
                !self
                    .bindings(*action)
                    .iter()
                    .any(|b| matches!(BindingSpec::parse(b), BindingSpec::Key(_)))
            })
            .collect()
    }

    /// Restore one action to its legacy default. Actions with no default become unbound.
    pub fn reset_action(&mut self, action: Action) {
        let defaults = Self::legacy_defaults();
        let name = legacy_name(action);
        match defaults.bindings.get(name) {
            Some(list) => {
                self.bindings.insert(name.to_owned(), list.clone());
            }
            None => {
                self.bindings.insert(name.to_owned(), Vec::new());
            }
        }
    }

    /// Restore every action to the legacy defaults. Unknown action entries are dropped.
    pub fn reset_all(&mut self) {
        *self = Self::legacy_defaults();
    }

    /// Every binding used by more than one action. Unknown action names and unknown
    /// binding strings are ignored, as they are at runtime.
    pub fn conflicts(&self) -> Vec<Conflict> {
        let mut by_binding: BTreeMap<String, Vec<Action>> = BTreeMap::new();
        for (name, list) in &self.bindings {
            let Some(action) = legacy_name_to_action(name) else {
                continue;
            };
            for binding in list {
                if matches!(BindingSpec::parse(binding), BindingSpec::Unknown(_)) {
                    continue;
                }
                let entry = by_binding.entry(binding.clone()).or_default();
                if !entry.contains(&action) {
                    entry.push(action);
                }
            }
        }
        by_binding
            .into_iter()
            .filter(|(_, actions)| actions.len() > 1)
            .map(|(binding, actions)| Conflict { binding, actions })
            .collect()
    }

    /// Serialize to the v1 file format (pretty JSON, keys sorted).
    pub fn to_json_string(&self) -> Result<String, InputMapError> {
        let value = serde_json::json!({
            "schemaVersion": SCHEMA_VERSION,
            "bindings": self.bindings,
        });
        Ok(serde_json::to_string_pretty(&value)?)
    }

    /// Parse the v1 file format. Unknown actions and binding strings are kept.
    pub fn from_json_str(text: &str) -> Result<Loaded, InputMapError> {
        let value: Value = serde_json::from_str(text)?;
        let version = value
            .get("schemaVersion")
            .and_then(Value::as_u64)
            .ok_or(InputMapError::SchemaVersion)?;
        if version != SCHEMA_VERSION {
            return Err(InputMapError::UnsupportedSchemaVersion(version));
        }
        let object = value
            .get("bindings")
            .and_then(Value::as_object)
            .ok_or(InputMapError::Bindings)?;

        let mut map = Self::empty();
        // serde_json keeps the last of two equal keys without a sign. The later entry is
        // used, as the spec requires, and the repeat is reported here.
        let mut warnings: Vec<String> = duplicate_object_keys(text)
            .into_iter()
            .map(|key| {
                format!("`{key}` appears more than once in the input map; the later entry is used")
            })
            .collect();
        for (name, list) in object {
            let items = list.as_array().ok_or(InputMapError::Bindings)?;
            let mut canon: Vec<String> = Vec::with_capacity(items.len());
            for item in items {
                let text = item.as_str().ok_or(InputMapError::Bindings)?;
                let c = normalize_binding(text);
                if canon.contains(&c) {
                    warnings.push(format!("{name}: duplicate binding `{text}` ignored"));
                } else {
                    canon.push(c);
                }
            }
            map.bindings.insert(name.clone(), canon);
        }
        for action in ESSENTIAL_ACTIONS {
            if map.bindings(*action).is_empty() {
                warnings.push(format!("{action} has no bindings in this file"));
            }
        }
        Ok(Loaded { map, warnings })
    }

    /// Apply new file content, or keep `current` if the content is malformed.
    /// Returns the map to use and the notices to show. A malformed file yields exactly
    /// one notice and the unchanged map.
    pub fn reload(current: &InputMap, text: &str) -> (InputMap, Vec<String>) {
        match Self::from_json_str(text) {
            Ok(loaded) => (loaded.map, loaded.warnings),
            Err(err) => (
                current.clone(),
                vec![format!(
                    "Input map file could not be read; keeping the last good map ({err})"
                )],
            ),
        }
    }
}

/// Reverse of [`legacy_name`]. Unknown names give `None`.
fn legacy_name_to_action(name: &str) -> Option<Action> {
    Action::from_legacy(name).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::{Response, Trigger, parse_keymap};
    use std::collections::HashSet;

    const KEYBOARD_FIXTURE: &str = include_str!("../tests/fixtures/keyboard.json");

    /// Defaults that the legacy keyboard map does not have (spec-only, M2-17). Each one
    /// is listed here so the legacy-compatibility test can tell them apart from typos.
    const NON_LEGACY_DEFAULTS: &[(Action, &str)] = &[
        (Action::Up, "key:W"),
        (Action::Down, "key:S"),
        (Action::Left, "key:A"),
        (Action::Right, "key:D"),
        (Action::Settings, "key:M"),
        (Action::CycleAudio, "key:E"),
        (Action::ToggleSubtitles, "key:C"),
        (Action::Exit, "key:Shift+Esc"),
        (Action::IncreaseSubtitleDelay, "key:]"),
        (Action::DecreaseSubtitleDelay, "key:["),
        (Action::ResetSubtitleDelay, "key:Alt+R"),
        (Action::Mute, "key:N"),
        (Action::SpeedUp, "key:Ctrl+Up"),
        (Action::SpeedDown, "key:Ctrl+Down"),
        (Action::ChapterNext, "key:Ctrl+PgDown"),
        (Action::ChapterPrevious, "key:Ctrl+PgUp"),
    ];

    #[test]
    fn legacy_names_round_trip_for_every_action() {
        for action in ALL_ACTIONS {
            assert_eq!(
                Action::from_legacy(legacy_name(*action)),
                Ok(*action),
                "{action}"
            );
        }
    }

    #[test]
    fn binding_strings_normalize_modifier_order() {
        assert_eq!(normalize_binding("key:Shift+Ctrl+P"), "key:Ctrl+Shift+P");
        assert_eq!(normalize_binding("key:Meta+Ctrl+F"), "key:Ctrl+Meta+F");
        assert_eq!(normalize_binding("pad:South"), "pad:South");
        assert_eq!(
            BindingSpec::parse("pad:Dpad Up"),
            BindingSpec::Unknown("pad:Dpad Up".to_owned())
        );
        assert!(matches!(
            BindingSpec::parse("key:Shift+"),
            BindingSpec::Unknown(_)
        ));
        assert!(matches!(BindingSpec::parse("P"), BindingSpec::Unknown(_)));
    }

    #[test]
    fn defaults_bind_every_legacy_keyboard_action() {
        // Every action the legacy keyboard map binds (in the Action vocabulary) must have
        // a default here. `exit` is one of them: legacy binds it as a long press of Esc,
        // and it gets Shift+Esc here (see the table above).
        let legacy = parse_keymap(KEYBOARD_FIXTURE).unwrap();
        let mut legacy_actions: HashSet<Action> = HashSet::new();
        for b in &legacy.bindings {
            match &b.response {
                Response::Press(list) => legacy_actions.extend(list.iter().copied()),
                Response::ShortLong { short, long } => {
                    legacy_actions.extend(short.iter().copied());
                    legacy_actions.extend(long.iter().copied());
                }
            }
        }
        let defaults = InputMap::legacy_defaults();
        let uncovered: Vec<Action> = ALL_ACTIONS
            .iter()
            .copied()
            .filter(|a| legacy_actions.contains(a) && defaults.bindings(*a).is_empty())
            .collect();
        assert!(
            uncovered.is_empty(),
            "legacy actions without a default: {uncovered:?}"
        );
    }

    #[test]
    fn every_default_binding_is_a_real_legacy_binding() {
        let legacy = parse_keymap(KEYBOARD_FIXTURE).unwrap();
        let defaults = InputMap::legacy_defaults();
        for action in ALL_ACTIONS {
            for text in defaults.bindings(*action) {
                if NON_LEGACY_DEFAULTS.contains(&(*action, text.as_str())) {
                    continue;
                }
                let BindingSpec::Key(chord) = BindingSpec::parse(text) else {
                    panic!("default `{text}` for {action} is not a key binding");
                };
                let mut found = false;
                for b in legacy
                    .bindings
                    .iter()
                    .filter(|b| b.trigger == Trigger::Key(chord.clone()))
                {
                    found |= match &b.response {
                        Response::Press(list) => list.contains(action),
                        Response::ShortLong { short, long } => {
                            short.contains(action) || long.contains(action)
                        }
                    };
                }
                assert!(
                    found,
                    "default `{text}` is not bound to {action} in the legacy map"
                );
            }
        }
    }

    #[test]
    fn non_legacy_defaults_are_exactly_the_listed_ones() {
        // The exemption list must match the table: a spec-only default that is not listed
        // fails the legacy test above, and a listed one that is no longer a default fails here.
        let defaults = InputMap::legacy_defaults();
        for (action, text) in NON_LEGACY_DEFAULTS {
            assert!(
                defaults.bindings(*action).iter().any(|b| b == text),
                "{action} no longer has the listed default {text}"
            );
        }
    }

    #[test]
    fn default_map_has_no_conflicts() {
        assert!(InputMap::legacy_defaults().conflicts().is_empty());
    }

    #[test]
    fn each_m2_17_action_has_its_documented_default() {
        let defaults = InputMap::legacy_defaults();
        let expected: &[(Action, &[&str])] = &[
            (Action::IncreaseVolume, &["key:="]),
            (Action::DecreaseVolume, &["key:-"]),
            (Action::Mute, &["key:N"]),
            (Action::SpeedUp, &["key:Ctrl+Up"]),
            (Action::SpeedDown, &["key:Ctrl+Down"]),
            (Action::ChapterNext, &["key:Ctrl+PgDown"]),
            (Action::ChapterPrevious, &["key:Ctrl+PgUp"]),
            (Action::ResetSubtitleDelay, &["key:Alt+R"]),
            (Action::IncreaseSubtitleDelay, &["key:Alt+S", "key:]"]),
            // Stored in canonical modifier order (Ctrl, Shift, Alt, Meta).
            (Action::DecreaseSubtitleDelay, &["key:Shift+Alt+S", "key:["]),
            (Action::Exit, &["key:Shift+Esc"]),
        ];
        for (action, bindings) in expected {
            let got: Vec<&str> = defaults
                .bindings(*action)
                .iter()
                .map(String::as_str)
                .collect();
            assert_eq!(&got, bindings, "{action}");
        }
    }

    #[test]
    fn pip_toggle_is_unbound_and_ctrl_shift_p_stays_play_pause() {
        let defaults = InputMap::legacy_defaults();
        let pip = Action::Host(HostAction::TogglePip);
        assert!(defaults.bindings(pip).is_empty());
        // Ctrl+Shift+P is the legacy play/pause alias. It must not move to PiP.
        assert_eq!(
            defaults.also_bound_to(pip, "key:Ctrl+Shift+P"),
            vec![Action::PlayPause]
        );
    }

    #[test]
    fn pip_toggle_can_be_bound_by_the_user() {
        let mut map = InputMap::legacy_defaults();
        let pip = Action::Host(HostAction::TogglePip);
        map.add_binding(pip, "key:F12").unwrap();
        assert_eq!(map.bindings(pip), &["key:F12".to_owned()]);
        assert!(map.conflicts().is_empty());
    }

    #[test]
    fn new_action_names_survive_a_save_and_load() {
        let mut map = InputMap::legacy_defaults();
        map.set_bindings(Action::SpeedUp, &["key:F5"]).unwrap();
        let text = map.to_json_string().unwrap();
        assert!(text.contains("\"speed_up\""));
        assert!(text.contains("\"host:togglePip\""));
        let loaded = InputMap::from_json_str(&text).unwrap();
        assert!(loaded.warnings.is_empty());
        assert_eq!(loaded.map, map);
    }

    #[test]
    fn repeated_action_key_in_file_uses_the_later_entry_and_warns() {
        let text = r#"{"schemaVersion":1,"bindings":{"home":["key:H"],"back":["key:Esc"],"home":["key:F1"]}}"#;
        let loaded = InputMap::from_json_str(text).unwrap();
        assert_eq!(loaded.map.bindings(Action::Home), &["key:F1".to_owned()]);
        assert_eq!(loaded.warnings.len(), 1, "{:?}", loaded.warnings);
        assert!(loaded.warnings[0].contains("`home`"));
        assert!(loaded.warnings[0].contains("later entry"));
    }

    #[test]
    fn repeated_action_key_warning_reaches_reload_notices() {
        let text = r#"{"schemaVersion":1,"bindings":{"back":["key:Esc"],"back":["key:B"]}}"#;
        let (map, notices) = InputMap::reload(&InputMap::legacy_defaults(), text);
        assert_eq!(map.bindings(Action::Back), &["key:B".to_owned()]);
        assert!(notices.iter().any(|n| n.contains("`back`")));
    }

    #[test]
    fn conflict_detection_reports_both_actions() {
        let mut map = InputMap::legacy_defaults();
        map.set_bindings(Action::Stop, &["key:P"]).unwrap();
        let conflicts = map.conflicts();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].binding, "key:P");
        assert!(conflicts[0].actions.contains(&Action::PlayPause));
        assert!(conflicts[0].actions.contains(&Action::Stop));
    }

    #[test]
    fn conflicts_ignore_modifier_order_and_unknown_entries() {
        let mut map = InputMap::legacy_defaults();
        // Ctrl+Shift+P is already play_pause; the same chord typed in another order
        // must be reported as a conflict, not treated as new.
        assert_eq!(
            map.also_bound_to(Action::Stop, "key:Shift+Ctrl+P"),
            vec![Action::PlayPause]
        );
        map.set_bindings(Action::Stop, &["key:Shift+Ctrl+P"])
            .unwrap();
        assert_eq!(map.conflicts().len(), 1);

        // Unknown action names and unknown binding strings never produce conflicts.
        let loaded = InputMap::from_json_str(
            r#"{"schemaVersion":1,"bindings":{"future_action":["key:Q"],"play_pause":["pad:South"],"stop":["pad:South"]}}"#,
        )
        .unwrap();
        assert_eq!(loaded.map.conflicts().len(), 1);
        assert_eq!(loaded.map.conflicts()[0].binding, "pad:South");
    }

    #[test]
    fn add_binding_enforces_validation_and_cap() {
        let mut map = InputMap::empty();
        map.add_binding(Action::Home, "key:H").unwrap();
        assert!(matches!(
            map.add_binding(Action::Home, "key:H"),
            Err(InputMapError::DuplicateBinding)
        ));
        assert!(matches!(
            map.add_binding(Action::Home, "key:"),
            Err(InputMapError::InvalidBinding(_))
        ));
        map.add_binding(Action::Home, "key:F1").unwrap();
        map.add_binding(Action::Home, "key:F2").unwrap();
        assert!(matches!(
            map.add_binding(Action::Home, "key:F3"),
            Err(InputMapError::TooManyBindings)
        ));
        assert_eq!(map.bindings(Action::Home).len(), MAX_USER_BINDINGS);
    }

    #[test]
    fn essential_action_keeps_its_last_binding() {
        let mut map = InputMap::legacy_defaults();
        map.set_bindings(Action::Back, &["key:Esc"]).unwrap();
        let err = map.remove_binding(Action::Back, "key:Esc").unwrap_err();
        assert_eq!(err.to_string(), "Keep at least one binding for Back");
        assert_eq!(map.bindings(Action::Back), &["key:Esc".to_owned()]);

        map.add_binding(Action::Back, "key:B").unwrap();
        map.remove_binding(Action::Back, "key:Esc").unwrap();
        assert_eq!(map.bindings(Action::Back), &["key:B".to_owned()]);

        assert!(matches!(
            map.set_bindings(Action::Back, &[]),
            Err(InputMapError::EssentialBinding(_))
        ));
        // A non-essential action may be cleared.
        map.set_bindings(Action::Home, &[]).unwrap();
        assert!(map.bindings(Action::Home).is_empty());
    }

    #[test]
    fn reset_action_and_reset_all_restore_defaults() {
        let defaults = InputMap::legacy_defaults();
        let mut map = defaults.clone();
        map.set_bindings(Action::Pause, &["key:F9"]).unwrap();
        map.reset_action(Action::Pause);
        assert_eq!(map, defaults);

        map.set_bindings(Action::Pause, &["key:F9"]).unwrap();
        map.reset_all();
        assert_eq!(map, defaults);
        assert_eq!(InputMap::default(), defaults);
    }

    #[test]
    fn defaults_round_trip_through_json() {
        let original = InputMap::legacy_defaults();
        let text = original.to_json_string().unwrap();
        let loaded = InputMap::from_json_str(&text).unwrap();
        assert!(loaded.warnings.is_empty());
        assert_eq!(loaded.map, original);
    }

    #[test]
    fn edited_map_with_unknown_entries_round_trips() {
        let mut map = InputMap::legacy_defaults();
        map.set_bindings(Action::Pause, &["key:F9", "pad:Start"])
            .unwrap();
        map.set_bindings(Action::Stop, &[]).unwrap();
        let text = map.to_json_string().unwrap();
        let reloaded = InputMap::from_json_str(&text).unwrap().map;
        assert_eq!(reloaded, map);
        assert_eq!(
            reloaded.bindings(Action::Pause),
            &["key:F9".to_owned(), "pad:Start".to_owned()]
        );
        assert!(reloaded.bindings(Action::Stop).is_empty());

        // An unknown action name survives a load and save cycle.
        let with_unknown = r#"{"schemaVersion":1,"bindings":{"future_action":["key:Q"]}}"#;
        let loaded = InputMap::from_json_str(with_unknown).unwrap().map;
        let saved = loaded.to_json_string().unwrap();
        assert!(saved.contains("future_action"));
    }

    #[test]
    fn duplicate_binding_in_file_keeps_one_and_warns() {
        let loaded =
            InputMap::from_json_str(r#"{"schemaVersion":1,"bindings":{"home":["key:H","key:H"]}}"#)
                .unwrap();
        assert_eq!(loaded.map.bindings(Action::Home), &["key:H".to_owned()]);
        assert!(
            loaded
                .warnings
                .iter()
                .any(|w| w.contains("duplicate binding"))
        );
    }

    #[test]
    fn duplicate_persisted_bindings_do_not_consume_the_unique_binding_limit() {
        let loaded = InputMap::from_json_str(
            r#"{"schemaVersion":1,"bindings":{"home":["key:H","key:H","key:H","key:H"]}}"#,
        )
        .expect("duplicates are ignored, not counted as additional bindings");
        assert_eq!(loaded.map.bindings(Action::Home), ["key:H"]);
        assert!(
            loaded
                .warnings
                .iter()
                .any(|warning| warning.contains("duplicate binding"))
        );
    }

    #[test]
    fn empty_essential_action_in_file_warns_but_loads() {
        let loaded =
            InputMap::from_json_str(r#"{"schemaVersion":1,"bindings":{"back":[]}}"#).unwrap();
        assert!(loaded.warnings.iter().any(|w| w.contains("Back")));
    }

    #[test]
    fn malformed_file_keeps_last_good_map_with_one_notice() {
        let current = InputMap::legacy_defaults();
        let (map, notices) = InputMap::reload(&current, "{ not json");
        assert_eq!(map, current);
        assert_eq!(notices.len(), 1);

        let (map, notices) =
            InputMap::reload(&current, r#"{"schemaVersion":1,"bindings":{"back":"Esc"}}"#);
        assert_eq!(map, current);
        assert_eq!(notices.len(), 1);
    }

    #[test]
    fn reload_applies_valid_content() {
        let current = InputMap::legacy_defaults();
        let (map, notices) = InputMap::reload(
            &current,
            r#"{"schemaVersion":1,"bindings":{"pause":["key:F9"]}}"#,
        );
        // The file does not mention `back`, so the only notice is the essential-action one.
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("Back"));
        assert_eq!(map.bindings(Action::Pause), &["key:F9".to_owned()]);
        assert!(map.bindings(Action::PlayPause).is_empty());
    }

    #[test]
    fn fullscreen_and_subtitle_actions_have_keyboard_defaults() {
        // M2-17 lists these explicitly; each must keep a `key:` default so the
        // keyboard-only path (M2-18) works for them.
        let defaults = InputMap::legacy_defaults();
        let required = [
            Action::Host(HostAction::Fullscreen),
            Action::CycleSubtitles,
            Action::ToggleSubtitles,
            Action::IncreaseSubtitleDelay,
            Action::DecreaseSubtitleDelay,
            Action::ResetSubtitleDelay,
        ];
        for action in required {
            assert!(
                defaults
                    .bindings(action)
                    .iter()
                    .any(|b| matches!(BindingSpec::parse(b), BindingSpec::Key(_))),
                "{action} has no keyboard default"
            );
        }
        assert!(
            defaults
                .bindings(Action::Host(HostAction::Fullscreen))
                .iter()
                .any(|b| b == "key:F11")
        );
    }

    /// Every action in [`ALL_ACTIONS`] whose bindings in `map` include `binding`.
    fn actions_for(map: &InputMap, binding: &str) -> Vec<Action> {
        ALL_ACTIONS
            .iter()
            .copied()
            .filter(|action| map.bindings(*action).iter().any(|b| b == binding))
            .collect()
    }

    #[test]
    fn wasd_keys_navigate_like_their_arrow_keys() {
        let defaults = InputMap::legacy_defaults();
        assert_eq!(actions_for(&defaults, "key:Up"), vec![Action::Up]);
        assert_eq!(actions_for(&defaults, "key:W"), vec![Action::Up]);
        assert_eq!(actions_for(&defaults, "key:Left"), vec![Action::Left]);
        assert_eq!(actions_for(&defaults, "key:A"), vec![Action::Left]);
        assert_eq!(actions_for(&defaults, "key:Down"), vec![Action::Down]);
        assert_eq!(actions_for(&defaults, "key:S"), vec![Action::Down]);
        assert_eq!(actions_for(&defaults, "key:Right"), vec![Action::Right]);
        assert_eq!(actions_for(&defaults, "key:D"), vec![Action::Right]);
    }

    #[test]
    fn m_opens_settings_and_moved_keys_keep_their_actions() {
        let defaults = InputMap::legacy_defaults();
        assert_eq!(actions_for(&defaults, "key:M"), vec![Action::Settings]);
        assert_eq!(actions_for(&defaults, "key:E"), vec![Action::CycleAudio]);
        assert_eq!(
            actions_for(&defaults, "key:C"),
            vec![Action::ToggleSubtitles]
        );
        assert_eq!(actions_for(&defaults, "key:N"), vec![Action::Mute]);
    }

    #[test]
    fn legacy_keys_keep_their_actions_next_to_wasd() {
        let defaults = InputMap::legacy_defaults();
        let up: Vec<&str> = defaults
            .bindings(Action::Up)
            .iter()
            .map(String::as_str)
            .collect();
        assert_eq!(up, ["key:Up", "key:Shift+Up", "key:W"]);
        let left: Vec<&str> = defaults
            .bindings(Action::Left)
            .iter()
            .map(String::as_str)
            .collect();
        assert_eq!(left, ["key:Left", "key:Shift+Left", "key:A"]);
        let down: Vec<&str> = defaults
            .bindings(Action::Down)
            .iter()
            .map(String::as_str)
            .collect();
        assert_eq!(down, ["key:Down", "key:Shift+Down", "key:S"]);
        assert_eq!(actions_for(&defaults, "key:Return"), vec![Action::Enter]);
        assert_eq!(actions_for(&defaults, "key:Esc"), vec![Action::Back]);
        assert_eq!(actions_for(&defaults, "key:P"), vec![Action::PlayPause]);
        assert_eq!(actions_for(&defaults, "key:Space"), vec![Action::PlayPause]);
    }

    #[test]
    fn replace_binding_keeps_position_and_count() {
        let mut map = InputMap::legacy_defaults();
        // Pause defaults to [Ctrl+P, Media Pause]; replacing the first keeps the second.
        map.replace_binding(Action::Pause, "key:Ctrl+P", "key:F9")
            .unwrap();
        assert_eq!(
            map.bindings(Action::Pause),
            &["key:F9".to_owned(), "key:Media Pause".to_owned()]
        );
    }

    #[test]
    fn replace_binding_refuses_a_duplicate_on_the_same_action() {
        let mut map = InputMap::legacy_defaults();
        // Media Pause is already the second binding of Pause.
        assert!(matches!(
            map.replace_binding(Action::Pause, "key:Ctrl+P", "key:Media Pause"),
            Err(InputMapError::DuplicateBinding)
        ));
        assert!(
            map.bindings(Action::Pause)
                .contains(&"key:Ctrl+P".to_owned())
        );
    }

    #[test]
    fn replace_binding_adds_when_the_old_binding_is_not_set() {
        let mut map = InputMap::legacy_defaults();
        map.replace_binding(Action::Home, "key:F12", "key:F1")
            .unwrap();
        assert_eq!(
            map.bindings(Action::Home),
            &["key:H".to_owned(), "key:F1".to_owned()]
        );
    }

    #[test]
    fn replace_binding_rejects_an_invalid_new_binding() {
        let mut map = InputMap::legacy_defaults();
        assert!(matches!(
            map.replace_binding(Action::Home, "key:H", "key:"),
            Err(InputMapError::InvalidBinding(_))
        ));
        assert_eq!(map.bindings(Action::Home), &["key:H".to_owned()]);
    }

    #[test]
    fn schema_version_must_be_one() {
        assert!(matches!(
            InputMap::from_json_str(r#"{"schemaVersion":2,"bindings":{}}"#),
            Err(InputMapError::UnsupportedSchemaVersion(2))
        ));
        assert!(matches!(
            InputMap::from_json_str(r#"{"bindings":{}}"#),
            Err(InputMapError::SchemaVersion)
        ));
        assert!(matches!(
            InputMap::from_json_str(r#"{"schemaVersion":1,"bindings":{"back":[1]}}"#),
            Err(InputMapError::Bindings)
        ));
    }
}

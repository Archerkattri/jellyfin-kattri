//! Gamepad backend contract. No SDL or OS dependency yet.
//!
//! A backend turns device-specific input into [`GamepadEvent`]s. The keymap
//! ([`crate::keymap::Keymap::lookup_button`]) then maps the button name to actions.
//! Backends for SDL (Linux, Windows, macOS) and for the web-gamepad bridge are
//! planned for M2; each lives behind this trait so the app never depends on SDL types.

/// A single button transition from a connected device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GamepadEvent {
    /// Stable device identifier chosen by the backend. Used to match a keymap's idmatcher.
    pub device: String,
    /// Button name in the form the keymap files use, for example `KEY_BUTTON_0`.
    pub button: String,
    pub pressed: bool,
}

/// Failure to poll a backend.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GamepadError {
    #[error("gamepad backend `{0}` is not available on this platform")]
    Unavailable(&'static str),
    #[error("gamepad backend failed: {0}")]
    Backend(String),
}

/// Something that reports gamepad button transitions.
///
/// `poll` must not block. The app calls it once per frame and drains the result.
pub trait GamepadBackend {
    /// Short backend name for logs and settings UI, for example `sdl` or `null`.
    fn name(&self) -> &'static str;

    /// Return button transitions since the previous call.
    fn poll(&mut self) -> Result<Vec<GamepadEvent>, GamepadError>;

    /// Number of controllers the backend can currently see. Backends that cannot tell
    /// keep the default of zero.
    fn connected_controllers(&self) -> usize {
        0
    }
}

/// A backend that never reports input. Used when gamepad support is disabled
/// (`input.sdl_gamepad_enabled = false`) and in tests.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullGamepad;

impl GamepadBackend for NullGamepad {
    fn name(&self) -> &'static str {
        "null"
    }

    fn poll(&mut self) -> Result<Vec<GamepadEvent>, GamepadError> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_backend_reports_nothing_and_never_fails() {
        let mut backend = NullGamepad;
        assert_eq!(backend.name(), "null");
        assert!(backend.poll().unwrap().is_empty());
        assert!(backend.poll().unwrap().is_empty());
    }

    #[test]
    fn backends_work_behind_a_trait_object() {
        let mut backend: Box<dyn GamepadBackend> = Box::new(NullGamepad);
        assert!(backend.poll().unwrap().is_empty());
    }

    #[test]
    fn null_backend_reports_no_controllers() {
        assert_eq!(NullGamepad.connected_controllers(), 0);
        let backend: Box<dyn GamepadBackend> = Box::new(NullGamepad);
        assert_eq!(backend.connected_controllers(), 0);
    }

    /// Trait double: returns scripted poll results in order, then nothing.
    struct ScriptedBackend {
        polls: std::collections::VecDeque<Result<Vec<GamepadEvent>, GamepadError>>,
    }

    impl GamepadBackend for ScriptedBackend {
        fn name(&self) -> &'static str {
            "scripted"
        }

        fn poll(&mut self) -> Result<Vec<GamepadEvent>, GamepadError> {
            self.polls.pop_front().unwrap_or_else(|| Ok(Vec::new()))
        }
    }

    fn press(button: &str) -> GamepadEvent {
        GamepadEvent {
            device: "test-pad".to_owned(),
            button: button.to_owned(),
            pressed: true,
        }
    }

    #[test]
    fn trait_double_hands_out_each_scripted_batch_once() {
        let mut backend: Box<dyn GamepadBackend> = Box::new(ScriptedBackend {
            polls: vec![
                Ok(vec![press("KEY_BUTTON_0")]),
                Err(GamepadError::Backend("link lost".to_owned())),
            ]
            .into(),
        });
        assert_eq!(backend.poll().unwrap(), vec![press("KEY_BUTTON_0")]);
        assert_eq!(
            backend.poll(),
            Err(GamepadError::Backend("link lost".to_owned()))
        );
        assert!(backend.poll().unwrap().is_empty());
        assert_eq!(backend.connected_controllers(), 0);
    }
}

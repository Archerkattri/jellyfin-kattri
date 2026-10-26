//! Audio output device model. Pure: it validates device entries, keeps the list and the
//! selection, and picks the value for mpv's `audio-device` property. It does not enumerate
//! real hardware and makes no mpv calls.
//!
//! Ids are the strings mpv itself reports for a device. A selected id must be in the list, so
//! the value handed to mpv always names a device the list knows about. With no selection the
//! value is `auto`, which lets mpv choose.

use std::collections::HashSet;
use std::fmt;

/// Longest device id, in bytes.
pub const DEVICE_ID_MAX_LEN: usize = 256;

/// Longest display name, in bytes.
pub const DEVICE_NAME_MAX_LEN: usize = 256;

/// The mpv value that lets mpv pick the output device. It is also the value used when no
/// device is selected, so it cannot be used as a device id.
pub const AUTO_DEVICE: &str = "auto";

/// Why a device entry or selection was refused. Messages are fixed phrases and never include
/// the id or name, so a device label cannot reach a log line through an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioDeviceError {
    /// The id is empty or only whitespace.
    EmptyId,
    /// The id is longer than [`DEVICE_ID_MAX_LEN`] bytes.
    IdTooLong,
    /// The id contains a control character.
    IdControlCharacter,
    /// The id is the reserved `auto` value.
    ReservedId,
    /// The name is empty or only whitespace.
    EmptyName,
    /// The name is longer than [`DEVICE_NAME_MAX_LEN`] bytes.
    NameTooLong,
    /// The name contains a control character.
    NameControlCharacter,
    /// Two entries in the list share an id.
    DuplicateId,
    /// The id being selected is not in the list.
    UnknownDevice,
}

impl fmt::Display for AudioDeviceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::EmptyId => "audio device id is empty",
            Self::IdTooLong => "audio device id is too long",
            Self::IdControlCharacter => "audio device id contains a control character",
            Self::ReservedId => "audio device id `auto` is reserved",
            Self::EmptyName => "audio device name is empty",
            Self::NameTooLong => "audio device name is too long",
            Self::NameControlCharacter => "audio device name contains a control character",
            Self::DuplicateId => "audio device list has a duplicate id",
            Self::UnknownDevice => "audio device is not in the device list",
        };
        f.write_str(text)
    }
}

impl std::error::Error for AudioDeviceError {}

/// One output device: the id mpv uses and the name a person reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioDevice {
    id: String,
    name: String,
}

impl AudioDevice {
    /// Build an entry. Refuses an id or name that is empty, too long, or has a control
    /// character. An id of `auto` is refused too.
    pub fn new(id: &str, name: &str) -> Result<Self, AudioDeviceError> {
        validate_id(id)?;
        validate_name(name)?;
        Ok(Self {
            id: id.to_owned(),
            name: name.to_owned(),
        })
    }

    /// The id mpv uses for this device.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The name shown to the person.
    pub fn name(&self) -> &str {
        &self.name
    }
}

fn validate_id(id: &str) -> Result<(), AudioDeviceError> {
    if id.trim().is_empty() {
        return Err(AudioDeviceError::EmptyId);
    }
    if id.len() > DEVICE_ID_MAX_LEN {
        return Err(AudioDeviceError::IdTooLong);
    }
    if id.chars().any(char::is_control) {
        return Err(AudioDeviceError::IdControlCharacter);
    }
    if id == AUTO_DEVICE {
        return Err(AudioDeviceError::ReservedId);
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<(), AudioDeviceError> {
    if name.trim().is_empty() {
        return Err(AudioDeviceError::EmptyName);
    }
    if name.len() > DEVICE_NAME_MAX_LEN {
        return Err(AudioDeviceError::NameTooLong);
    }
    if name.chars().any(char::is_control) {
        return Err(AudioDeviceError::NameControlCharacter);
    }
    Ok(())
}

/// The device list and the chosen device. A selection, when present, always names a device in
/// the list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AudioDeviceList {
    devices: Vec<AudioDevice>,
    selected: Option<String>,
}

impl AudioDeviceList {
    /// Build a list with nothing selected. Refuses two entries with the same id.
    pub fn new(devices: Vec<AudioDevice>) -> Result<Self, AudioDeviceError> {
        check_unique(&devices)?;
        Ok(Self {
            devices,
            selected: None,
        })
    }

    /// The entries, in the order they were given.
    pub fn devices(&self) -> &[AudioDevice] {
        &self.devices
    }

    /// The selected device id, or `None` when mpv should choose.
    pub fn selected_id(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    /// Select a device by id. Refuses an id that is not in the list, and keeps the previous
    /// selection in that case.
    pub fn select(&mut self, id: &str) -> Result<(), AudioDeviceError> {
        if !self.devices.iter().any(|device| device.id == id) {
            return Err(AudioDeviceError::UnknownDevice);
        }
        self.selected = Some(id.to_owned());
        Ok(())
    }

    /// Drop the selection, so mpv chooses the device again.
    pub fn clear_selection(&mut self) {
        self.selected = None;
    }

    /// Replace the entries after a fresh device scan. The selection survives when its device
    /// is still present, and is cleared otherwise. A refused list leaves everything unchanged.
    pub fn replace_devices(&mut self, devices: Vec<AudioDevice>) -> Result<(), AudioDeviceError> {
        check_unique(&devices)?;
        let kept = self
            .selected
            .as_ref()
            .filter(|id| devices.iter().any(|device| device.id == **id))
            .cloned();
        self.devices = devices;
        self.selected = kept;
        Ok(())
    }

    /// The value for mpv's `audio-device` property: the selected id, or `auto` when none is
    /// selected.
    pub fn mpv_value(&self) -> &str {
        self.selected.as_deref().unwrap_or(AUTO_DEVICE)
    }
}

fn check_unique(devices: &[AudioDevice]) -> Result<(), AudioDeviceError> {
    let mut seen = HashSet::with_capacity(devices.len());
    for device in devices {
        if !seen.insert(device.id.as_str()) {
            return Err(AudioDeviceError::DuplicateId);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: &str, name: &str) -> AudioDevice {
        AudioDevice::new(id, name).expect("test device is valid")
    }

    fn sample_list() -> AudioDeviceList {
        AudioDeviceList::new(vec![
            device("alsa/default", "Default output"),
            device("alsa/hdmi", "HDMI output"),
        ])
        .expect("ids are unique")
    }

    #[test]
    fn valid_entry_keeps_its_id_and_name() {
        let entry = AudioDevice::new("wasapi/{abc}", "Speakers").expect("valid");
        assert_eq!(entry.id(), "wasapi/{abc}");
        assert_eq!(entry.name(), "Speakers");
    }

    #[test]
    fn empty_and_whitespace_ids_are_refused() {
        assert_eq!(
            AudioDevice::new("", "Speakers"),
            Err(AudioDeviceError::EmptyId)
        );
        assert_eq!(
            AudioDevice::new("   ", "Speakers"),
            Err(AudioDeviceError::EmptyId)
        );
    }

    #[test]
    fn id_length_limit_is_inclusive() {
        let at_limit = "a".repeat(DEVICE_ID_MAX_LEN);
        assert!(AudioDevice::new(&at_limit, "Speakers").is_ok());
        let past_limit = "a".repeat(DEVICE_ID_MAX_LEN + 1);
        assert_eq!(
            AudioDevice::new(&past_limit, "Speakers"),
            Err(AudioDeviceError::IdTooLong)
        );
    }

    #[test]
    fn ids_with_control_characters_are_refused() {
        for bad in ["alsa/default\n", "alsa\tdefault", "alsa\0"] {
            assert_eq!(
                AudioDevice::new(bad, "Speakers"),
                Err(AudioDeviceError::IdControlCharacter),
                "accepted {bad:?}"
            );
        }
    }

    #[test]
    fn auto_is_reserved_and_cannot_be_a_device_id() {
        assert_eq!(
            AudioDevice::new("auto", "Speakers"),
            Err(AudioDeviceError::ReservedId)
        );
    }

    #[test]
    fn names_must_be_present_bounded_and_printable() {
        assert_eq!(
            AudioDevice::new("alsa/default", ""),
            Err(AudioDeviceError::EmptyName)
        );
        assert_eq!(
            AudioDevice::new("alsa/default", "  "),
            Err(AudioDeviceError::EmptyName)
        );
        let past_limit = "n".repeat(DEVICE_NAME_MAX_LEN + 1);
        assert_eq!(
            AudioDevice::new("alsa/default", &past_limit),
            Err(AudioDeviceError::NameTooLong)
        );
        assert_eq!(
            AudioDevice::new("alsa/default", "Speakers\r\nBroken"),
            Err(AudioDeviceError::NameControlCharacter)
        );
    }

    #[test]
    fn duplicate_ids_are_refused_when_building_the_list() {
        let result = AudioDeviceList::new(vec![
            device("alsa/default", "One"),
            device("alsa/default", "Two"),
        ]);
        assert_eq!(result, Err(AudioDeviceError::DuplicateId));
    }

    #[test]
    fn default_list_selects_nothing_and_reports_auto() {
        let list = AudioDeviceList::default();
        assert_eq!(list.selected_id(), None);
        assert_eq!(list.mpv_value(), "auto");
        assert!(list.devices().is_empty());

        let with_devices = sample_list();
        assert_eq!(with_devices.selected_id(), None);
        assert_eq!(with_devices.mpv_value(), "auto");
    }

    #[test]
    fn selecting_a_listed_device_sets_the_mpv_value_to_its_id() {
        let mut list = sample_list();
        list.select("alsa/hdmi").expect("device is listed");
        assert_eq!(list.selected_id(), Some("alsa/hdmi"));
        assert_eq!(list.mpv_value(), "alsa/hdmi");
    }

    #[test]
    fn selecting_an_unlisted_device_is_refused_and_keeps_the_old_choice() {
        let mut list = sample_list();
        list.select("alsa/default").expect("device is listed");
        assert_eq!(
            list.select("alsa/missing"),
            Err(AudioDeviceError::UnknownDevice)
        );
        assert_eq!(list.selected_id(), Some("alsa/default"));

        let mut empty = AudioDeviceList::default();
        assert_eq!(
            empty.select("alsa/default"),
            Err(AudioDeviceError::UnknownDevice)
        );
        assert_eq!(empty.mpv_value(), "auto");
    }

    #[test]
    fn clearing_the_selection_returns_to_auto() {
        let mut list = sample_list();
        list.select("alsa/hdmi").expect("device is listed");
        list.clear_selection();
        assert_eq!(list.selected_id(), None);
        assert_eq!(list.mpv_value(), "auto");
    }

    #[test]
    fn refresh_keeps_a_selection_that_is_still_listed() {
        let mut list = sample_list();
        list.select("alsa/hdmi").expect("device is listed");
        list.replace_devices(vec![
            device("alsa/hdmi", "HDMI output"),
            device("alsa/usb", "USB headset"),
        ])
        .expect("ids are unique");
        assert_eq!(list.selected_id(), Some("alsa/hdmi"));
        assert_eq!(list.devices().len(), 2);
    }

    #[test]
    fn refresh_clears_a_selection_whose_device_disappeared() {
        let mut list = sample_list();
        list.select("alsa/hdmi").expect("device is listed");
        list.replace_devices(vec![device("alsa/default", "Default output")])
            .expect("ids are unique");
        assert_eq!(list.selected_id(), None);
        assert_eq!(list.mpv_value(), "auto");
    }

    #[test]
    fn refresh_with_duplicate_ids_leaves_the_list_unchanged() {
        let mut list = sample_list();
        list.select("alsa/hdmi").expect("device is listed");
        let before = list.clone();
        let result =
            list.replace_devices(vec![device("alsa/usb", "One"), device("alsa/usb", "Two")]);
        assert_eq!(result, Err(AudioDeviceError::DuplicateId));
        assert_eq!(list, before);
    }

    #[test]
    fn error_messages_never_include_the_device_id_or_name() {
        let secret_id = "alsa/secret-serial-1234\n";
        let err = AudioDevice::new(secret_id, "Speakers")
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(!err.contains("secret"), "error leaked the id: {err}");
        assert!(!err.is_empty());
    }
}

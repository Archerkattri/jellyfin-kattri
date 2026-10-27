//! The Windows display backend: `EnumDisplaySettingsW` and `ChangeDisplaySettingsExW` on the
//! primary display. Changes use `CDS_FULLSCREEN`, which Windows undoes by itself when the process
//! ends, so even a crash that skips the saved record leaves the display on its normal mode.
//!
//! Windows reports whole hertz here, so 23.976 Hz shows as 24 Hz and 59.94 Hz as 60 Hz (or 59);
//! the matching tolerance in `display_match` covers that.

use kattri_client_core::display_match::{DisplayBackend, DisplayMode};
use windows_sys::Win32::Graphics::Gdi::{
    CDS_FULLSCREEN, ChangeDisplaySettingsExW, DEVMODEW, DISP_CHANGE_BADDUALVIEW,
    DISP_CHANGE_BADFLAGS, DISP_CHANGE_BADMODE, DISP_CHANGE_BADPARAM, DISP_CHANGE_FAILED,
    DISP_CHANGE_NOTUPDATED, DISP_CHANGE_RESTART, DISP_CHANGE_SUCCESSFUL, DM_DISPLAYFREQUENCY,
    DM_PELSHEIGHT, DM_PELSWIDTH, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW,
};

/// Modes below this refresh rate are the "hardware default" markers (0 and 1 Hz), not real rates.
const MIN_REAL_HZ: u32 = 20;
/// A display lists a few hundred modes at most; the cap stops a faulty driver from looping us.
const MAX_MODES: u32 = 4096;

pub(super) struct PrimaryDisplay;

fn blank() -> DEVMODEW {
    // SAFETY: DEVMODEW is plain data; all-zero is a valid (empty) value.
    let mut mode: DEVMODEW = unsafe { std::mem::zeroed() };
    mode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
    mode
}

fn mode_from(raw: &DEVMODEW) -> DisplayMode {
    DisplayMode::new(
        raw.dmPelsWidth,
        raw.dmPelsHeight,
        raw.dmDisplayFrequency.saturating_mul(1000),
    )
}

fn describe(code: i32) -> &'static str {
    match code {
        DISP_CHANGE_RESTART => "the computer must restart for this mode",
        DISP_CHANGE_FAILED => "the display driver failed",
        DISP_CHANGE_BADMODE => "the display does not support this mode",
        DISP_CHANGE_NOTUPDATED => "the mode could not be written to the registry",
        DISP_CHANGE_BADFLAGS => "invalid flags",
        DISP_CHANGE_BADPARAM => "invalid parameter",
        DISP_CHANGE_BADDUALVIEW => "the change is not possible in dual-view mode",
        _ => "unknown error",
    }
}

impl DisplayBackend for PrimaryDisplay {
    type Error = String;

    fn current_mode(&mut self) -> Result<DisplayMode, String> {
        let mut raw = blank();
        // SAFETY: `raw` is a valid DEVMODEW with dmSize set; a null device name selects the
        // primary display.
        let ok = unsafe { EnumDisplaySettingsW(std::ptr::null(), ENUM_CURRENT_SETTINGS, &mut raw) };
        if ok == 0 {
            return Err("could not read the current display mode".to_owned());
        }
        Ok(mode_from(&raw))
    }

    fn available_modes(&mut self) -> Result<Vec<DisplayMode>, String> {
        let mut modes = Vec::new();
        for index in 0..MAX_MODES {
            let mut raw = blank();
            // SAFETY: as above; the index selects the next listed mode.
            let ok = unsafe { EnumDisplaySettingsW(std::ptr::null(), index, &mut raw) };
            if ok == 0 {
                break;
            }
            let mode = mode_from(&raw);
            if mode.refresh_millihz >= MIN_REAL_HZ * 1000 && !modes.contains(&mode) {
                modes.push(mode);
            }
        }
        if modes.is_empty() {
            return Err("the display lists no usable modes".to_owned());
        }
        Ok(modes)
    }

    fn set_mode(&mut self, mode: DisplayMode) -> Result<(), String> {
        let mut raw = blank();
        // Start from the current settings so every field the driver needs is filled in.
        // SAFETY: as above.
        if unsafe { EnumDisplaySettingsW(std::ptr::null(), ENUM_CURRENT_SETTINGS, &mut raw) } == 0 {
            return Err("could not read the current display mode".to_owned());
        }
        raw.dmPelsWidth = mode.width;
        raw.dmPelsHeight = mode.height;
        raw.dmDisplayFrequency = mode.refresh_millihz.div_ceil(1000).max(1);
        raw.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY;
        // SAFETY: `raw` is a fully initialised DEVMODEW that outlives the call; a null device
        // name is the primary display, and no window or parameter is passed.
        let code = unsafe {
            ChangeDisplaySettingsExW(
                std::ptr::null(),
                &raw,
                std::ptr::null_mut(),
                CDS_FULLSCREEN,
                std::ptr::null(),
            )
        };
        if code == DISP_CHANGE_SUCCESSFUL {
            Ok(())
        } else {
            Err(format!("{mode}: {}", describe(code)))
        }
    }
}

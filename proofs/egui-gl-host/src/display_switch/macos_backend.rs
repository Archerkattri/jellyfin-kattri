//! Core Graphics display-mode backend for the main macOS display.
//!
//! `CGDisplaySetDisplayMode` scopes a requested mode to this process and restores the saved
//! Displays preference when the process exits, including an abnormal exit. The host still stores
//! its original mode so its normal stop and exit paths restore promptly, and startup recovery can
//! reconcile a record left by an interrupted session.

use std::{ffi::c_void, ptr};

use super::{ModeProperties, display_mode_from_values, is_safe_target_mode};
use kattri_client_core::display_match::{DisplayBackend, DisplayMode};

type DisplayId = u32;
type DisplayModeRef = *const c_void;
type CfArrayRef = *const c_void;
type CfDictionaryRef = *const c_void;

const MAX_DISPLAY_MODES: isize = 4096;
const CG_ERROR_SUCCESS: i32 = 0;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGMainDisplayID() -> DisplayId;
    fn CGDisplayIsActive(display: DisplayId) -> i32;
    fn CGDisplayIsOnline(display: DisplayId) -> i32;
    fn CGDisplayIsInMirrorSet(display: DisplayId) -> i32;
    fn CGDisplayCopyDisplayMode(display: DisplayId) -> DisplayModeRef;
    fn CGDisplayCopyAllDisplayModes(display: DisplayId, options: CfDictionaryRef) -> CfArrayRef;
    fn CGDisplaySetDisplayMode(
        display: DisplayId,
        mode: DisplayModeRef,
        options: CfDictionaryRef,
    ) -> i32;
    fn CGDisplayModeGetWidth(mode: DisplayModeRef) -> usize;
    fn CGDisplayModeGetHeight(mode: DisplayModeRef) -> usize;
    fn CGDisplayModeGetPixelWidth(mode: DisplayModeRef) -> usize;
    fn CGDisplayModeGetPixelHeight(mode: DisplayModeRef) -> usize;
    fn CGDisplayModeGetRefreshRate(mode: DisplayModeRef) -> f64;
    fn CGDisplayModeGetIOFlags(mode: DisplayModeRef) -> u32;
    fn CGDisplayModeIsUsableForDesktopGUI(mode: DisplayModeRef) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFArrayGetCount(array: CfArrayRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CfArrayRef, index: isize) -> *const c_void;
    fn CFRelease(value: *const c_void);
}

/// Owns a retained Core Foundation or Core Graphics object.
struct OwnedCf(*const c_void);

impl OwnedCf {
    fn new(value: *const c_void, description: &'static str) -> Result<Self, String> {
        if value.is_null() {
            Err(description.to_owned())
        } else {
            Ok(Self(value))
        }
    }

    fn as_ptr(&self) -> *const c_void {
        self.0
    }
}

impl Drop for OwnedCf {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: constructors only wrap retained objects returned by CGDisplayCopy* calls.
            unsafe { CFRelease(self.0) };
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct PrimaryDisplay;

fn primary_display_id() -> Result<DisplayId, String> {
    // SAFETY: CGMainDisplayID has no preconditions and returns the current main display ID.
    let display = unsafe { CGMainDisplayID() };
    if display == 0 {
        return Err("Core Graphics did not report a main display".to_owned());
    }
    // SAFETY: the ID came from Core Graphics and is used only for immediate status queries.
    let active = unsafe { CGDisplayIsActive(display) };
    // SAFETY: same as above.
    let online = unsafe { CGDisplayIsOnline(display) };
    if active == 0 || online == 0 {
        return Err("the main display is not active and online".to_owned());
    }
    Ok(display)
}

fn copy_current_mode(display: DisplayId) -> Result<OwnedCf, String> {
    // SAFETY: `display` is a currently active main display ID.
    OwnedCf::new(
        unsafe { CGDisplayCopyDisplayMode(display) },
        "Core Graphics could not read the main display mode",
    )
}

fn copy_modes(display: DisplayId) -> Result<OwnedCf, String> {
    // SAFETY: `display` is active; the API specifies null options for the current mode list.
    OwnedCf::new(
        unsafe { CGDisplayCopyAllDisplayModes(display, ptr::null()) },
        "Core Graphics could not enumerate main display modes",
    )
}

fn mode_count(modes: &OwnedCf) -> Result<isize, String> {
    // SAFETY: `modes` owns a non-null CFArray returned by Core Graphics.
    let count = unsafe { CFArrayGetCount(modes.as_ptr()) };
    if count <= 0 {
        return Err("the main display exposes no modes".to_owned());
    }
    if count > MAX_DISPLAY_MODES {
        return Err("the main display mode list exceeds the safety limit".to_owned());
    }
    Ok(count)
}

fn mode_properties(mode: DisplayModeRef) -> Result<ModeProperties, String> {
    if mode.is_null() {
        return Err("Core Graphics returned an empty display mode".to_owned());
    }
    // SAFETY: `mode` is a live mode reference from a Core Graphics copy or its retained array.
    let width = unsafe { CGDisplayModeGetWidth(mode) };
    // SAFETY: same as above.
    let height = unsafe { CGDisplayModeGetHeight(mode) };
    // SAFETY: same as above.
    let refresh_hz = unsafe { CGDisplayModeGetRefreshRate(mode) };
    let display_mode = display_mode_from_values(width, height, refresh_hz)
        .ok_or_else(|| "the display mode has invalid dimensions or refresh rate".to_owned())?;
    // SAFETY: same as above.
    let pixel_width = unsafe { CGDisplayModeGetPixelWidth(mode) };
    // SAFETY: same as above.
    let pixel_height = unsafe { CGDisplayModeGetPixelHeight(mode) };
    if pixel_width == 0 || pixel_height == 0 {
        return Err("the display mode has invalid pixel dimensions".to_owned());
    }
    // SAFETY: same as above.
    let io_flags = unsafe { CGDisplayModeGetIOFlags(mode) };
    // SAFETY: same as above.
    let usable_for_desktop = unsafe { CGDisplayModeIsUsableForDesktopGUI(mode) } != 0;
    Ok(ModeProperties {
        display_mode,
        pixel_width,
        pixel_height,
        io_flags,
        usable_for_desktop,
    })
}

fn find_matching_mode(
    modes: &OwnedCf,
    wanted: DisplayMode,
    current: &ModeProperties,
) -> Result<DisplayModeRef, String> {
    let count = mode_count(modes)?;
    let mut found = None;
    for index in 0..count {
        // SAFETY: index is bounded by the retained CFArray's checked count.
        let raw = unsafe { CFArrayGetValueAtIndex(modes.as_ptr(), index) };
        if raw.is_null() {
            continue;
        }
        let mode = match mode_properties(raw) {
            Ok(mode) => mode,
            Err(_) => continue,
        };
        if !is_safe_target_mode(&mode, wanted, current) {
            continue;
        }
        if found.is_some() {
            return Err("the requested display mode is ambiguous; no change was made".to_owned());
        }
        found = Some(raw);
    }
    found.ok_or_else(|| {
        format!(
            "the requested mode is unavailable with the current pixel dimensions and display flags: {wanted}"
        )
    })
}

impl DisplayBackend for PrimaryDisplay {
    type Error = String;

    fn current_mode(&mut self) -> Result<DisplayMode, String> {
        let display = primary_display_id()?;
        let current = copy_current_mode(display)?;
        Ok(mode_properties(current.as_ptr())?.display_mode)
    }

    fn available_modes(&mut self) -> Result<Vec<DisplayMode>, String> {
        let display = primary_display_id()?;
        let modes = copy_modes(display)?;
        let count = mode_count(&modes)?;
        let capacity = usize::try_from(count)
            .map_err(|_| "the main display mode count is invalid".to_owned())?;
        let mut available = Vec::with_capacity(capacity);
        for index in 0..count {
            // SAFETY: index is bounded by the retained CFArray's checked count.
            let raw = unsafe { CFArrayGetValueAtIndex(modes.as_ptr(), index) };
            if raw.is_null() {
                continue;
            }
            let Ok(mode) = mode_properties(raw) else {
                continue;
            };
            if mode.usable_for_desktop && !available.contains(&mode.display_mode) {
                available.push(mode.display_mode);
            }
        }
        if available.is_empty() {
            return Err(
                "the main display has no usable modes with a known refresh rate".to_owned(),
            );
        }
        Ok(available)
    }

    fn set_mode(&mut self, wanted: DisplayMode) -> Result<(), String> {
        let display = primary_display_id()?;
        // Core Graphics can change all outputs in a mirroring set as a side effect. Fail closed
        // rather than altering displays the refresh matcher did not select.
        // SAFETY: `display` is a currently active main display ID.
        if unsafe { CGDisplayIsInMirrorSet(display) } != 0 {
            return Err("mode switching is disabled while the main display is mirrored".to_owned());
        }
        let current = copy_current_mode(display)?;
        let current_properties = mode_properties(current.as_ptr())?;
        let modes = copy_modes(display)?;
        let target = find_matching_mode(&modes, wanted, &current_properties)?;
        // SAFETY: `target` is a borrowed mode from `modes`, which remains alive for this call.
        let result = unsafe { CGDisplaySetDisplayMode(display, target, ptr::null()) };
        if result == CG_ERROR_SUCCESS {
            Ok(())
        } else {
            Err(format!(
                "Core Graphics refused the requested display mode ({result})"
            ))
        }
    }
}

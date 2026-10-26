//! Sample presence guest for the `kattri-plugin` world in `../../wit/kattri-plugin.wit`.
//!
//! Build from the repository root with the wasm32-wasip2 target installed:
//!
//! ```text
//! cargo build --release --manifest-path crates/kattri-plugins/examples/sample-presence/Cargo.toml --target wasm32-wasip2 --target-dir target
//! ```
//!
//! The artifact is `target/wasm32-wasip2/release/sample_presence.wasm`. The guest is `no_std`
//! so that it imports only the host functions in the WIT world. The std library would add
//! WASI CLI imports (stdio, environment, exit) that the host does not provide.
//!
//! The host grants `presence.write` and `log.write`. Presence payloads use a fixed text, not
//! the item title, because the title is shared only when the user turns it on.

#![no_std]

extern crate alloc;

use core::alloc::{GlobalAlloc, Layout};
use core::panic::PanicInfo;
use core::ptr::null_mut;
use core::sync::atomic::{AtomicUsize, Ordering};

wit_bindgen::generate!({
    path: "../../wit",
    world: "kattri-plugin",
});

const WASM_PAGE: usize = 65_536;

/// Bump allocator for the guest heap. The guest allocates only when the host passes the plugin
/// id to `init` (once per load) and never on a playback event, so memory is never reused
/// without harming the sample. Each call is already capped by the host's memory limit.
struct Bump;

static NEXT: AtomicUsize = AtomicUsize::new(0);
static END: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Bump {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if END.load(Ordering::Relaxed) == 0 {
            // Start the heap after the data and stack already in linear memory.
            let base = core::arch::wasm32::memory_size::<0>() * WASM_PAGE;
            NEXT.store(base, Ordering::Relaxed);
            END.store(base, Ordering::Relaxed);
        }
        let start = align_up(NEXT.load(Ordering::Relaxed), layout.align());
        let Some(end) = start.checked_add(layout.size()) else {
            return null_mut();
        };
        if end > END.load(Ordering::Relaxed) {
            let shortfall = end - END.load(Ordering::Relaxed);
            let pages = shortfall.div_ceil(WASM_PAGE);
            if core::arch::wasm32::memory_grow::<0>(pages) == usize::MAX {
                return null_mut();
            }
            END.store(
                core::arch::wasm32::memory_size::<0>() * WASM_PAGE,
                Ordering::Relaxed,
            );
        }
        NEXT.store(end, Ordering::Relaxed);
        start as *mut u8
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
}

fn align_up(value: usize, align: usize) -> usize {
    value.div_ceil(align) * align
}

#[global_allocator]
static ALLOCATOR: Bump = Bump;

/// The component model calls this to pass strings and lists into the guest. wit-bindgen gets
/// it from std on wasip2, and the no_std guest must define it over its own allocator.
///
/// # Safety
///
/// Called only by the canonical ABI with a pointer and length from a previous call, or with
/// `old_len == 0` for a fresh allocation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cabi_realloc(
    old_ptr: *mut u8,
    old_len: usize,
    align: usize,
    new_len: usize,
) -> *mut u8 {
    if old_len == 0 && new_len == 0 {
        return align as *mut u8;
    }
    let new_ptr = unsafe { ALLOCATOR.alloc(Layout::from_size_align_unchecked(new_len, align)) };
    if new_ptr.is_null() {
        core::arch::wasm32::unreachable();
    }
    if old_len != 0 {
        let keep = if old_len < new_len { old_len } else { new_len };
        unsafe { core::ptr::copy_nonoverlapping(old_ptr, new_ptr, keep) };
    }
    new_ptr
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    // An endless loop runs out of the per-call fuel, so the host traps the call.
    loop {}
}

const IDLE_PAYLOAD: &str = r#"{"details":"Watching","state":"Idle"}"#;
const PLAYING_PAYLOAD: &str = r#"{"details":"Watching","state":"Playing"}"#;
const PAUSED_PAYLOAD: &str = r#"{"details":"Watching","state":"Paused"}"#;

struct SamplePresence;

export!(SamplePresence);

impl Guest for SamplePresence {
    fn init(plugin_id: alloc::string::String) -> Result<(), alloc::string::String> {
        // A refused log line must not stop startup, so its result is dropped.
        let _ = log_write("sample-presence started");
        drop(plugin_id);
        presence_write(IDLE_PAYLOAD)
    }

    fn on_playback_event(event: PlaybackEvent) {
        let payload = match event {
            PlaybackEvent::Started | PlaybackEvent::Resumed => PLAYING_PAYLOAD,
            PlaybackEvent::Paused => PAUSED_PAYLOAD,
            PlaybackEvent::Stopped => IDLE_PAYLOAD,
            // Position updates do not change the presence text, and the guest keeps no clock.
            PlaybackEvent::Position(_) => return,
        };
        // The host refuses a write it cannot perform. The guest does not retry.
        let _ = presence_write(payload);
    }

    fn shutdown() {}
}

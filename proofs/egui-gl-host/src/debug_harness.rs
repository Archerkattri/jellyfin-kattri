//! Debug-build QA harness for the native host. Compiled only with `debug_assertions`, so
//! release builds do not contain it, and inert unless `KATTRI_DEBUG_DIR` is set
//! (scripts/run_debug_signin.ps1 sets it).
//!
//! Control channel: write lines to `<KATTRI_DEBUG_DIR>/commands.txt` (the host reads and
//! deletes the file each frame). One synthetic input batch runs per frame, in order:
//!   move X Y | click X Y | rclick X Y | dblclick X Y | scroll DX DY | key NAME [ctrl|shift|alt]
//!   text STRING | resize W H | shot | tree
//! Coordinates are egui points in window space. `tree` writes `ui-tree.txt` from the
//! AccessKit tree (role, bounds and label of every widget); `shot` writes `screenshot.png`.
//! Creating `<KATTRI_DEBUG_DIR>/screenshot.request` also requests a screenshot.
//! Results are appended to `kattri-debug.log`.
//!
//! The OS screen capture cannot see this OpenGL window, so a screenshot is read back from the
//! GL context instead.

use glow::HasContext;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

static SHOT_REQUESTED: AtomicBool = AtomicBool::new(false);
static QUEUE: Mutex<VecDeque<Vec<egui::Event>>> = Mutex::new(VecDeque::new());
static TREE: Mutex<BTreeMap<u64, String>> = Mutex::new(BTreeMap::new());
static LAST_VALUES: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

fn debug_dir() -> Option<PathBuf> {
    std::env::var_os("KATTRI_DEBUG_DIR").map(PathBuf::from)
}

fn log(message: &str) {
    jellyfin_egui_client::debug_log::log(message);
}

/// Logs `key: value` once per change, so per-frame state can be watched without flooding the log.
pub fn log_if_changed(key: &str, value: &str) {
    let mut guard = LAST_VALUES.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    if map.get(key).map(String::as_str) != Some(value) {
        map.insert(key.to_owned(), value.to_owned());
        log(&format!("{key}: {value}"));
    }
}

fn modifiers(tokens: &[&str]) -> egui::Modifiers {
    let mut mods = egui::Modifiers::NONE;
    for token in tokens {
        match token.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods.ctrl = true,
            "shift" => mods.shift = true,
            "alt" => mods.alt = true,
            _ => {}
        }
    }
    mods
}

fn button(pos: egui::Pos2, which: egui::PointerButton, pressed: bool) -> egui::Event {
    egui::Event::PointerButton {
        pos,
        button: which,
        pressed,
        modifiers: egui::Modifiers::NONE,
    }
}

/// Turns one command line into input batches (one per frame) or runs a non-input command.
fn parse_command(line: &str, window: &winit::window::Window) {
    let parts: Vec<&str> = line.split_whitespace().collect();
    let Some((&command, args)) = parts.split_first() else {
        return;
    };
    let number = |index: usize| args.get(index).and_then(|value| value.parse::<f32>().ok());
    let point = || Some(egui::pos2(number(0)?, number(1)?));
    let mut batches: Vec<Vec<egui::Event>> = Vec::new();
    match command {
        "move" => {
            if let Some(pos) = point() {
                batches.push(vec![egui::Event::PointerMoved(pos)]);
            }
        }
        "click" | "dblclick" | "rclick" => {
            if let Some(pos) = point() {
                let which = if command == "rclick" {
                    egui::PointerButton::Secondary
                } else {
                    egui::PointerButton::Primary
                };
                let times = if command == "dblclick" { 2 } else { 1 };
                batches.push(vec![egui::Event::PointerMoved(pos)]);
                for _ in 0..times {
                    batches.push(vec![button(pos, which, true)]);
                    batches.push(vec![button(pos, which, false)]);
                }
            }
        }
        "scroll" => {
            if let (Some(dx), Some(dy)) = (number(0), number(1)) {
                batches.push(vec![egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: egui::vec2(dx, dy),
                    phase: egui::TouchPhase::Move,
                    source: egui::MouseWheelSource::Unknown,
                    modifiers: egui::Modifiers::NONE,
                }]);
            }
        }
        "key" => {
            let key = args.first().and_then(|name| egui::Key::from_name(name));
            match key {
                Some(key) => {
                    let mods = modifiers(&args[1..]);
                    for pressed in [true, false] {
                        batches.push(vec![egui::Event::Key {
                            key,
                            physical_key: Some(key),
                            pressed,
                            repeat: false,
                            modifiers: mods,
                        }]);
                    }
                }
                None => log(&format!("command error: unknown key in `{line}`")),
            }
        }
        "text" => {
            let text = line
                .trim_start()
                .strip_prefix("text")
                .unwrap_or("")
                .trim_start();
            batches.push(vec![egui::Event::Text(text.to_owned())]);
        }
        "resize" => {
            if let (Some(w), Some(h)) = (number(0), number(1)) {
                let _ = window.request_inner_size(winit::dpi::LogicalSize::new(w, h));
            }
        }
        "shot" => SHOT_REQUESTED.store(true, Ordering::Relaxed),
        "tree" => dump_tree(),
        "metrics" => match super::qa_metrics::dump() {
            Ok(()) => log("qa metrics: written"),
            Err(_) => log("qa metrics: write failed"),
        },
        other => log(&format!("command error: unknown command `{other}`")),
    }
    QUEUE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .extend(batches);
}

/// An idle window paints nothing, so `commands.txt` and `screenshot.request` would wait for
/// the next real input. While the harness is active the host wakes on this deadline instead.
pub fn poll_deadline() -> Option<std::time::Instant> {
    debug_dir()?;
    Some(std::time::Instant::now() + std::time::Duration::from_millis(150))
}

/// Reads `commands.txt` if present and adds one pending input batch to `input.events`.
pub fn inject(input: &mut egui::RawInput, window: &winit::window::Window, ctx: &egui::Context) {
    let Some(dir) = debug_dir() else { return };
    ctx.enable_accesskit();
    // A window started from a background launcher can come up minimized, and a minimized window
    // gets no paint events. Restore and focus it once.
    static RESTORED: AtomicBool = AtomicBool::new(false);
    if !RESTORED.swap(true, Ordering::Relaxed) {
        window.set_minimized(false);
        window.focus_window();
    }
    let commands = dir.join("commands.txt");
    if let Ok(text) = std::fs::read_to_string(&commands) {
        let _ = std::fs::remove_file(&commands);
        for line in text.lines() {
            log(&format!("command: {line}"));
            parse_command(line, window);
        }
    }
    let batch = QUEUE.lock().unwrap_or_else(|e| e.into_inner()).pop_front();
    if let Some(events) = batch {
        input.events.extend(events);
    }
}

/// Keeps the latest AccessKit node descriptions from each frame's output.
pub fn observe_tree(platform: &egui::PlatformOutput) {
    let Some(update) = platform.accesskit_update.as_ref() else {
        return;
    };
    let mut tree = TREE.lock().unwrap_or_else(|e| e.into_inner());
    for (id, node) in &update.nodes {
        let bounds = node
            .bounds()
            .map(|b| {
                format!(
                    "[{:.0},{:.0} {:.0}x{:.0}]",
                    b.x0,
                    b.y0,
                    b.x1 - b.x0,
                    b.y1 - b.y0
                )
            })
            .unwrap_or_default();
        let label = node.label().or_else(|| node.value()).unwrap_or("");
        tree.insert(id.0, format!("{:?} {bounds} \"{label}\"", node.role()));
    }
}

fn dump_tree() {
    let Some(dir) = debug_dir() else { return };
    let tree = TREE.lock().unwrap_or_else(|e| e.into_inner());
    let mut text = String::new();
    for line in tree.values() {
        text.push_str(line);
        text.push('\n');
    }
    let outcome = std::fs::write(dir.join("ui-tree.txt"), text);
    log(&format!(
        "ui tree: {} nodes ({})",
        tree.len(),
        if outcome.is_ok() {
            "ok"
        } else {
            "write failed"
        }
    ));
}

/// Writes a PNG of the current framebuffer and, when supplied, the offscreen video framebuffer.
/// Call after libmpv and egui render, before swapping the window buffers.
pub fn capture_if_requested(
    gl: &std::sync::Arc<glow::Context>,
    width: u32,
    height: u32,
    video: Option<&super::video_target::VideoTarget>,
) {
    let Some(dir) = debug_dir() else { return };
    let request = dir.join("screenshot.request");
    let wanted = SHOT_REQUESTED.swap(false, Ordering::Relaxed) || request.exists();
    if !wanted || width == 0 || height == 0 {
        return;
    }
    let _ = std::fs::remove_file(&request);
    if let Some(video) = video {
        let _state = super::gl_adapter::GlStateSnapshot::capture(std::sync::Arc::clone(gl));
        let [video_width, video_height] = video.size();
        // SAFETY: this name belongs to the current context's video target.
        unsafe {
            gl.bind_framebuffer(
                glow::FRAMEBUFFER,
                std::num::NonZeroU32::new(video.framebuffer_id()).map(glow::NativeFramebuffer),
            )
        };
        capture_framebuffer(gl, &dir, "video-screenshot.png", video_width, video_height);
    }
    capture_framebuffer(gl, &dir, "screenshot.png", width, height);
}

fn capture_framebuffer(
    gl: &glow::Context,
    dir: &std::path::Path,
    name: &str,
    width: u32,
    height: u32,
) {
    if width == 0 || height == 0 {
        return;
    }

    let stride = width as usize * 4;
    let mut pixels = vec![0u8; stride * height as usize];
    // SAFETY: the host's GL context is current while a frame is presented, and the buffer is
    // exactly width * height * 4 bytes for an RGBA read.
    unsafe {
        gl.pixel_store_i32(glow::PACK_ALIGNMENT, 1);
        gl.read_pixels(
            0,
            0,
            width as i32,
            height as i32,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelPackData::Slice(Some(&mut pixels)),
        );
    }
    // GL rows run bottom to top and the alpha channel is not meaningful for a window.
    let mut upright = Vec::with_capacity(pixels.len());
    for row in pixels.chunks_exact(stride).rev() {
        upright.extend_from_slice(row);
    }
    for alpha in upright.iter_mut().skip(3).step_by(4) {
        *alpha = 255;
    }
    let png = encode_png(width, height, &upright);
    let target = dir.join(name);
    let outcome = std::fs::write(&target, png);
    log(&format!(
        "screenshot {width}x{height} -> {} ({})",
        target.display(),
        if outcome.is_ok() {
            "ok"
        } else {
            "write failed"
        }
    ));
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn adler32(bytes: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in bytes {
        a = (a + u32::from(byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let mut body = kind.to_vec();
    body.extend_from_slice(data);
    out.extend_from_slice(&body);
    out.extend_from_slice(&crc32(&body).to_be_bytes());
}

/// An RGBA PNG with stored (uncompressed) deflate blocks. Large but dependency-free.
fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let stride = width as usize * 4;
    let mut raw = Vec::with_capacity((stride + 1) * height as usize);
    for row in rgba.chunks_exact(stride) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    let mut zlib = vec![0x78, 0x01];
    let mut blocks = raw.chunks(65_535).peekable();
    while let Some(block) = blocks.next() {
        zlib.push(u8::from(blocks.peek().is_none()));
        zlib.extend_from_slice(&(block.len() as u16).to_le_bytes());
        zlib.extend_from_slice(&(!(block.len() as u16)).to_le_bytes());
        zlib.extend_from_slice(block);
    }
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = Vec::new();
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    header.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &header);
    chunk(&mut out, b"IDAT", &zlib);
    chunk(&mut out, b"IEND", &[]);
    out
}

#[cfg(test)]
mod tests {
    use super::{adler32, crc32, encode_png};

    #[test]
    fn checksums_match_known_values() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }

    #[test]
    fn png_has_signature_and_ends_with_iend() {
        let png = encode_png(2, 2, &[255; 16]);
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
    }
}

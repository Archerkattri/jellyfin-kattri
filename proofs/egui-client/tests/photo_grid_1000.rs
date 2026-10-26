#![cfg(feature = "photo-counters")]

use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use image::{ExtendedColorType, codecs::jpeg::JpegEncoder};
use jellyfin_egui_client::KattriApp;
use kattri_client_core::photo_grid::{
    DEFAULT_OVERSCAN_SCREENS, DecodeCounters, GridSpec, PhotoGrid,
};

struct GeneratedFolder(PathBuf);

impl GeneratedFolder {
    fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        for attempt in 0..100_u32 {
            let path = std::env::temp_dir().join(format!(
                "kattri-photo-grid-1000-{}-{stamp:x}-{attempt}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create generated photo folder: {error}"),
            }
        }
        panic!("could not reserve a unique generated photo folder")
    }
}

fn counters_dump_path() -> PathBuf {
    if let Some(path) = std::env::var_os("KATTRI_PHOTO_COUNTERS_PATH") {
        return PathBuf::from(path);
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    std::env::temp_dir().join(format!(
        "kattri-photo-counters-{}-{stamp:x}.json",
        std::process::id()
    ))
}

fn write_counters_dump(path: PathBuf, json: &[u8]) -> std::io::Result<PathBuf> {
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    output.write_all(json)?;
    output.sync_all()?;
    Ok(path)
}

impl Drop for GeneratedFolder {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn jpeg_fixture() -> Vec<u8> {
    let mut bytes = Vec::new();
    JpegEncoder::new(&mut bytes)
        .encode(
            &[
                0x31, 0x67, 0xA2, 0xFF, 0xC8, 0x24, 0x7D, 0xB0, 0x44, 0x82, 0x19, 0xED,
            ],
            2,
            2,
            ExtendedColorType::Rgb8,
        )
        .expect("encode JPEG fixture");
    bytes
}

#[test]
fn generated_thousand_jpeg_folder_decodes_only_requested_windows() {
    const PHOTO_COUNT: usize = 1_000;
    const CACHE_CAPACITY: usize = 64;
    let folder = GeneratedFolder::new();
    let fixture = jpeg_fixture();
    for index in 0..PHOTO_COUNT {
        fs::write(folder.0.join(format!("photo-{index:04}.jpg")), &fixture)
            .expect("write generated JPEG");
    }

    let mut paths: Vec<PathBuf> = fs::read_dir(&folder.0)
        .expect("read generated photo folder")
        .map(|entry| entry.expect("read generated photo entry").path())
        .collect();
    paths.sort();
    assert_eq!(paths.len(), PHOTO_COUNT);
    assert!(
        paths
            .iter()
            .all(|path| path.extension().is_some_and(|ext| ext == "jpg"))
    );

    let spec = GridSpec::new(800, 600, 200, 150).expect("valid grid");
    let mut grid = PhotoGrid::new(PHOTO_COUNT, spec, DEFAULT_OVERSCAN_SCREENS, CACHE_CAPACITY)
        .expect("photo grid");
    let actual_decode_counters = DecodeCounters::default();
    let mut peak_step_decodes = 0;
    let mut peak_window_len = 0;

    for row in 0..PHOTO_COUNT / spec.columns() {
        let plan = grid.set_scroll(row * spec.cell_height);
        let window = grid.window();
        let visible = grid.visible();
        peak_window_len = peak_window_len.max(window.len());
        let before = grid.decode_counters().decoded_total;
        for index in plan.requests {
            assert!(
                window.contains(&index),
                "request {index} escaped the near-visible window"
            );
            let bytes = fs::read(&paths[index]).expect("read generated JPEG");
            let (width, height, rgba) = KattriApp::decode_photo_counter_fixture(
                bytes,
                index,
                visible.contains(&index),
                &actual_decode_counters,
            )
            .expect("decode generated JPEG through production photo decoder");
            assert_eq!((width, height), (2, 2));
            assert_eq!(rgba.len(), 2 * 2 * 4);
            let completion = grid.complete(index);
            assert!(matches!(
                completion,
                kattri_client_core::photo_grid::Completion::Cached { .. }
            ));
        }
        let decoded_this_step = usize::try_from(grid.decode_counters().decoded_total - before)
            .expect("step decode count fits usize");
        peak_step_decodes = peak_step_decodes.max(decoded_this_step);
        assert!(grid.cached_len() <= CACHE_CAPACITY);
    }

    let counters = actual_decode_counters.snapshot();
    assert_eq!(counters.decoded_total, PHOTO_COUNT as u64);
    assert_eq!(counters.decoded_indices.len(), PHOTO_COUNT);
    assert!(peak_step_decodes <= peak_window_len);
    assert!(
        peak_window_len <= 48,
        "window exceeded visible plus overscan bound"
    );

    let dump = serde_json::json!({
        "decoded_total": counters.decoded_total,
        "decoded_visible": counters.decoded_visible,
        "decoded_prefetch": counters.decoded_prefetch,
        "cancelled": counters.cancelled,
        "decoded_indices": counters.decoded_indices,
        "peak_step_decodes": peak_step_decodes,
        "peak_window_len": peak_window_len,
    });
    let json = serde_json::to_vec(&dump).expect("serialize decode counters");
    let path = counters_dump_path();
    let written = write_counters_dump(path, &json)
        .unwrap_or_else(|error| panic!("write photo counters JSON dump: {error}"));
    eprintln!("photo decode counters JSON: {}", written.display());
}

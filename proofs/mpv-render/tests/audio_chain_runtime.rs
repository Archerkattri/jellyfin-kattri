//! Offline measurement of the production audio filter chain in a real mpv runtime.
//!
//! This is the M6-08 output measurement the option tests could not provide: it builds the
//! chain with the production builder ([`audio_filter_value`]), hands it to a trusted mpv
//! executable as `--af`, plays lavfi-generated PCM into `ao=pcm`, and measures the WAV the
//! runtime wrote. No audio device is opened, nothing is downloaded, and nothing is installed.
//!
//! Scope of the claim: this measures the numeric output of the real runtime. It is *not*
//! acoustic evidence and says nothing about speakers, receivers, DACs, hearing, or any
//! hardware passthrough path.
//!
//! Ignored by default because it needs an mpv executable with lavfi and `ao=pcm`:
//!
//! ```text
//! KATTRI_MPV_EXE=path/to/mpv.exe cargo test -p mpv-render-proof \
//!     --test audio_chain_runtime -- --ignored --nocapture
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;

use kattri_client_core::audio_filters::{AudioFilterSettings, Normalization};
use kattri_client_core::equalizer::Equalizer;
use mpv_render_proof::playback_policy::audio_filter_value;

/// Band index 1 is 62 Hz, the centre the generated tone uses.
const BOOSTED_BAND_DB: f64 = 12.0;
const TONE_HZ: u32 = 62;

fn mpv_executable() -> PathBuf {
    let Some(path) = std::env::var_os("KATTRI_MPV_EXE") else {
        panic!("set KATTRI_MPV_EXE to an mpv executable that provides lavfi and ao=pcm");
    };
    let path = PathBuf::from(path);
    assert!(
        path.is_file(),
        "KATTRI_MPV_EXE is not a file: {}",
        path.display()
    );
    path
}

fn no_filters() -> AudioFilterSettings {
    AudioFilterSettings {
        normalization: Normalization::new(false, -16.0).expect("loudness target"),
        pitch_correction: false,
        compression: false,
    }
}

/// Render one second of the generated tone through `af` (or directly) and return the WAV path.
fn render(mpv: &Path, directory: &Path, name: &str, af: Option<&str>) -> PathBuf {
    let wav = directory.join(format!("{name}.wav"));
    let mut command = Command::new(mpv);
    command.args([
        &format!("av://lavfi:sine=frequency={TONE_HZ}:duration=1"),
        "--no-config",
        "--no-video",
        "--ao=pcm",
        "--really-quiet",
    ]);
    command.arg(format!("--ao-pcm-file={}", wav.display()));
    if let Some(af) = af {
        command.arg(format!("--af={af}"));
    }
    let output = command.output().expect("spawn mpv");
    assert!(
        output.status.success(),
        "mpv failed: status={:?} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(wav.is_file(), "mpv did not write {}", wav.display());
    wav
}

fn chunk(bytes: &[u8], id: &[u8; 4]) -> Option<(usize, usize)> {
    let mut offset = 12;
    while offset + 8 <= bytes.len() {
        let name = &bytes[offset..offset + 4];
        let size = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().ok()?) as usize;
        let body = offset + 8;
        if name == id {
            return Some((body, size.min(bytes.len().saturating_sub(body))));
        }
        offset = body.checked_add(size)?.checked_add(size % 2)?;
    }
    None
}

/// Root-mean-square of the 16-bit PCM body, mixed to mono when needed.
fn read_wav_rms(path: &Path) -> (f64, usize) {
    let bytes = std::fs::read(path).expect("read wav");
    assert!(bytes.starts_with(b"RIFF"), "not a RIFF file");
    let (fmt_start, _) = chunk(&bytes, b"fmt ").expect("fmt chunk");
    let channels =
        u16::from_le_bytes(bytes[fmt_start + 2..fmt_start + 4].try_into().unwrap()) as usize;
    let bits = u16::from_le_bytes(bytes[fmt_start + 14..fmt_start + 16].try_into().unwrap());
    assert_eq!(bits, 16, "ao=pcm must write 16-bit samples");
    assert!(channels >= 1, "no channels");
    let (data_start, data_len) = chunk(&bytes, b"data").expect("data chunk");
    let samples: Vec<i16> = bytes[data_start..data_start + data_len]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| i16::from_le_bytes(*pair))
        .collect();
    let frames = samples.len() / channels;
    let mixed: Vec<f64> = samples
        .chunks_exact(channels)
        .map(|frame| frame.iter().map(|&sample| f64::from(sample)).sum::<f64>() / channels as f64)
        .collect();
    let rms = (mixed.iter().map(|s| s * s).sum::<f64>() / mixed.len() as f64).sqrt();
    (rms, frames)
}

#[test]
#[ignore = "requires an mpv executable with lavfi and ao=pcm named by KATTRI_MPV_EXE"]
fn production_equalizer_chain_raises_the_band_energy_in_real_output() {
    let mpv = mpv_executable();
    let directory = std::env::temp_dir().join(format!("kattri-audio-chain-{}", std::process::id()));
    std::fs::create_dir_all(&directory).expect("temp directory");

    // The chain comes from the production builder, not from a hand-written filter string.
    let mut gains = [0.0; 10];
    gains[1] = BOOSTED_BAND_DB;
    let boosted = Equalizer::from_gains(gains).expect("in-range gains");
    let chain = audio_filter_value(&boosted, &no_filters()).expect("boosted chain");
    assert!(
        chain.starts_with("lavfi=["),
        "unexpected production chain: {chain}"
    );
    // Bypass: a flat equalizer with no other filters builds no chain at all.
    assert_eq!(
        audio_filter_value(&Equalizer::flat(), &no_filters()),
        None,
        "a flat equalizer must not produce a filter graph"
    );

    let plain = render(&mpv, &directory, "plain", None);
    let filtered = render(&mpv, &directory, "filtered", Some(&chain));
    let (plain_rms, plain_frames) = read_wav_rms(&plain);
    let (filtered_rms, filtered_frames) = read_wav_rms(&filtered);

    println!("chain={chain}");
    println!("plain_rms={plain_rms:.1} filtered_rms={filtered_rms:.1}");
    assert!(plain_rms > 100.0, "generated tone was silent: {plain_rms}");
    assert_eq!(
        plain_frames, filtered_frames,
        "the filter chain must not change the duration"
    );
    let gain_db = 20.0 * (filtered_rms / plain_rms).log10();
    println!("measured_gain_db={gain_db:.2}");
    assert!(
        (6.0..=14.0).contains(&gain_db),
        "expected roughly the configured {BOOSTED_BAND_DB} dB boost, measured {gain_db:.2} dB"
    );

    std::fs::remove_dir_all(&directory).ok();
}

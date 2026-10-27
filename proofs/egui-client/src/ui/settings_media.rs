//! Playback, audio, subtitle and video pages of the settings screen.
//!
//! Each page reads the active profile's values, draws the controls on them, and hands the
//! result to `finish_page`, which saves only when a value changed.

use super::ui_text;
use egui::{RichText, Ui};
use kattri_client_core::equalizer::{BAND_FREQUENCIES_HZ, Preset};
use kattri_i18n::ids;
use kattri_settings::schema::{MAX_VOLUME_PERCENT_CHOICES, OUTRO_DELAY_MS_MAX};
use kattri_settings::{
    AspectMode, AudioOutput, BACKBUFFER_MB_MAX, CACHE_MB_MAX, CACHE_MB_MIN, ChannelLayout,
    DecodeMode, READAHEAD_SECS_MAX, SyncMode, ToneMapping,
};
use kattri_settings::{
    AudioSettings, EQUALIZER_BANDS, EQUALIZER_GAIN_LIMIT_DB, LOUDNESS_TARGET_LUFS_RANGE,
};

use crate::app::KattriApp;
use crate::theme::Palette;
use crate::ui::settings::{choice, finish_page, muted, settings_card};

/// The `sub-ass-override` spellings mpv takes, in the order the picker lists them.
const ASS_OVERRIDES: &[&str] = &["no", "yes", "force", "scale", "strip"];

fn sync_modes() -> [(SyncMode, &'static str); 3] {
    [
        (
            SyncMode::Audio,
            ui_text(ids::SETTINGS_MEDIA_SYNC_AUDIO_CLOCK),
        ),
        (
            SyncMode::DisplayResample,
            ui_text(ids::SETTINGS_MEDIA_SYNC_DISPLAY_RESAMPLE),
        ),
        (
            SyncMode::DisplayAdrop,
            ui_text(ids::SETTINGS_MEDIA_SYNC_DISPLAY_ADROP),
        ),
    ]
}

fn audio_outputs() -> [(AudioOutput, &'static str); 3] {
    [
        (
            AudioOutput::Basic,
            ui_text(ids::SETTINGS_MEDIA_AUDIO_OUTPUT_BASIC),
        ),
        (
            AudioOutput::Spdif,
            ui_text(ids::SETTINGS_MEDIA_AUDIO_OUTPUT_SPDIF),
        ),
        (
            AudioOutput::Hdmi,
            ui_text(ids::SETTINGS_MEDIA_AUDIO_OUTPUT_HDMI),
        ),
    ]
}

fn channel_layouts() -> [(ChannelLayout, &'static str); 4] {
    [
        (
            ChannelLayout::Auto,
            ui_text(ids::SETTINGS_MEDIA_LAYOUT_AUTO),
        ),
        (
            ChannelLayout::Stereo,
            ui_text(ids::SETTINGS_MEDIA_LAYOUT_STEREO),
        ),
        (
            ChannelLayout::Surround51,
            ui_text(ids::SETTINGS_MEDIA_LAYOUT_SURROUND51),
        ),
        (
            ChannelLayout::Surround71,
            ui_text(ids::SETTINGS_MEDIA_LAYOUT_SURROUND71),
        ),
    ]
}

fn max_volume_labels() -> [(u32, &'static str); 4] {
    [
        (
            MAX_VOLUME_PERCENT_CHOICES[0],
            ui_text(ids::SETTINGS_MEDIA_VOLUME_100),
        ),
        (
            MAX_VOLUME_PERCENT_CHOICES[1],
            ui_text(ids::SETTINGS_MEDIA_VOLUME_150),
        ),
        (
            MAX_VOLUME_PERCENT_CHOICES[2],
            ui_text(ids::SETTINGS_MEDIA_VOLUME_200),
        ),
        (
            MAX_VOLUME_PERCENT_CHOICES[3],
            ui_text(ids::SETTINGS_MEDIA_VOLUME_300),
        ),
    ]
}

fn decode_modes() -> [(DecodeMode, &'static str); 7] {
    [
        (DecodeMode::Auto, ui_text(ids::SETTINGS_MEDIA_DECODE_AUTO)),
        (DecodeMode::Off, ui_text(ids::SETTINGS_MEDIA_DECODE_OFF)),
        (
            DecodeMode::D3d11va,
            ui_text(ids::SETTINGS_MEDIA_DECODE_D3D11VA),
        ),
        (DecodeMode::Nvdec, ui_text(ids::SETTINGS_MEDIA_DECODE_NVDEC)),
        (DecodeMode::Vaapi, ui_text(ids::SETTINGS_MEDIA_DECODE_VAAPI)),
        (
            DecodeMode::Videotoolbox,
            ui_text(ids::SETTINGS_MEDIA_DECODE_VIDEOTOOLBOX),
        ),
        (
            DecodeMode::Vulkan,
            ui_text(ids::SETTINGS_MEDIA_DECODE_VULKAN),
        ),
    ]
}

fn aspect_modes() -> [(AspectMode, &'static str); 8] {
    [
        (
            AspectMode::Custom,
            ui_text(ids::SETTINGS_MEDIA_ASPECT_CUSTOM),
        ),
        (
            AspectMode::Normal,
            ui_text(ids::SETTINGS_MEDIA_ASPECT_NORMAL),
        ),
        (AspectMode::Zoom, ui_text(ids::SETTINGS_MEDIA_ASPECT_ZOOM)),
        (
            AspectMode::Force4x3,
            ui_text(ids::SETTINGS_MEDIA_ASPECT_FORCE4X3),
        ),
        (
            AspectMode::Force16x9,
            ui_text(ids::SETTINGS_MEDIA_ASPECT_FORCE16X9),
        ),
        (
            AspectMode::Force16x9IfFourByThree,
            ui_text(ids::SETTINGS_MEDIA_ASPECT_FORCE16X9_4X3),
        ),
        (
            AspectMode::Stretch,
            ui_text(ids::SETTINGS_MEDIA_ASPECT_STRETCH),
        ),
        (
            AspectMode::NoScaling,
            ui_text(ids::SETTINGS_MEDIA_ASPECT_NO_SCALING),
        ),
    ]
}

pub(super) fn playback_page(ui: &mut Ui, app: &mut KattriApp, colors: Palette) {
    let before = app.settings.settings().clone();
    settings_card(ui, |ui| {
        let playback = &mut app.settings.settings_mut().playback;
        choice(
            ui,
            ui_text(ids::SETTINGS_MEDIA_SYNC_MODE_HEADING),
            "settings_sync_mode",
            &mut playback.sync_mode,
            &sync_modes(),
        );
        ui.checkbox(
            &mut playback.frame_drop,
            ui_text(ids::SETTINGS_MEDIA_FRAME_DROP),
        );
        ui.add(
            egui::Slider::new(&mut playback.default_speed, 0.25..=4.0)
                .text(ui_text(ids::SETTINGS_MEDIA_DEFAULT_SPEED)),
        );
        ui.add(
            egui::Slider::new(&mut playback.cache_mb, CACHE_MB_MIN..=CACHE_MB_MAX)
                .text(ui_text(ids::SETTINGS_MEDIA_CACHE_READ)),
        );
        ui.add(
            egui::Slider::new(&mut playback.demuxer_backbuffer_mb, 0..=BACKBUFFER_MB_MAX)
                .text(ui_text(ids::SETTINGS_MEDIA_CACHE_BACK_BUFFER)),
        );
        ui.add(
            egui::Slider::new(&mut playback.demuxer_readahead_secs, 0..=READAHEAD_SECS_MAX)
                .text(ui_text(ids::SETTINGS_MEDIA_CACHE_READ_AHEAD)),
        );
        ui.checkbox(
            &mut playback.skip_intro,
            ui_text(ids::SETTINGS_MEDIA_INTRO_SHOW_BUTTON),
        );
        ui.checkbox(
            &mut playback.auto_skip_intro,
            ui_text(ids::SETTINGS_MEDIA_INTRO_AUTO),
        );
        ui.checkbox(
            &mut playback.skip_outro,
            ui_text(ids::SETTINGS_MEDIA_CREDITS_SHOW_BUTTON),
        );
        ui.checkbox(
            &mut playback.auto_skip_outro,
            ui_text(ids::SETTINGS_MEDIA_CREDITS_AUTO),
        );
        ui.add(
            egui::Slider::new(&mut playback.outro_delay_ms, 0..=OUTRO_DELAY_MS_MAX)
                .step_by(1_000.0)
                .suffix(format!(" {}", ui_text(ids::SETTINGS_MEDIA_UNIT_MS)))
                .text(ui_text(ids::SETTINGS_MEDIA_CREDITS_DELAY)),
        );
        ui.add_enabled(
            false,
            egui::Checkbox::new(
                &mut playback.external_player,
                ui_text(ids::SETTINGS_MEDIA_EXTERNAL_PLAYER),
            ),
        );
        muted(
            ui,
            colors,
            "Unavailable: external-player handoff is not implemented in this build.",
            12.0,
        );

        let mut megabits = playback.max_streaming_bitrate / 1_000_000;
        let previous = megabits;
        ui.add(
            egui::Slider::new(&mut megabits, 0..=200).text(ui_text(ids::SETTINGS_MEDIA_BITRATE)),
        );
        if megabits != previous {
            playback.max_streaming_bitrate = megabits.saturating_mul(1_000_000);
        }
        ui.add_space(8.0);
        muted(ui, colors, ui_text(ids::SETTINGS_MEDIA_CACHE_HELP), 12.0);
    });
    finish_page(ui, app, colors, &before);
}

pub(super) fn audio_page(ui: &mut Ui, app: &mut KattriApp, colors: Palette) {
    let before = app.settings.settings().clone();
    settings_card(ui, |ui| {
        let audio = &mut app.settings.settings_mut().audio;
        ui.add_enabled_ui(false, |ui| {
            choice(
                ui,
                ui_text(ids::SETTINGS_MEDIA_AUDIO_OUTPUT_HEADING),
                "settings_audio_output",
                &mut audio.output,
                &audio_outputs(),
            );
        });
        muted(
            ui,
            colors,
            "Unavailable: output mode selection is not connected to the audio backend.",
            12.0,
        );
        choice(
            ui,
            ui_text(ids::SETTINGS_MEDIA_AUDIO_CHANNEL_LAYOUT),
            "settings_audio_channels",
            &mut audio.channels,
            &channel_layouts(),
        );
        ui.horizontal(|ui| {
            let caption = ui.label(ui_text(ids::SETTINGS_MEDIA_AUDIO_OUTPUT_DEVICE));
            if app.audio_devices.is_empty() {
                ui.text_edit_singleline(&mut audio.device);
            } else {
                let automatic = ui_text(ids::SETTINGS_MEDIA_AUDIO_DEVICE_AUTOMATIC);
                let shown = if audio.device == "auto" {
                    automatic.to_owned()
                } else {
                    audio.device.clone()
                };
                egui::ComboBox::from_id_salt("settings_audio_device")
                    .selected_text(shown)
                    .width(320.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut audio.device, "auto".to_owned(), automatic);
                        for device in &app.audio_devices {
                            ui.selectable_value(&mut audio.device, device.clone(), device);
                        }
                    })
                    .response
                    .labelled_by(caption.id);
            }
        });
        muted(
            ui,
            colors,
            ui_text(ids::SETTINGS_MEDIA_AUDIO_DEVICE_HELP),
            12.0,
        );
        ui.checkbox(
            &mut audio.normalize,
            ui_text(ids::SETTINGS_MEDIA_AUDIO_NORMALIZE),
        );
        ui.checkbox(
            &mut audio.gapless,
            ui_text(ids::SETTINGS_MEDIA_AUDIO_GAPLESS),
        );
        let exclusive_available = cfg!(windows) || cfg!(target_os = "macos");
        ui.add_enabled(
            exclusive_available,
            egui::Checkbox::new(
                &mut audio.exclusive,
                ui_text(ids::SETTINGS_MEDIA_AUDIO_EXCLUSIVE),
            ),
        );
        if !exclusive_available {
            muted(
                ui,
                colors,
                "Exclusive audio is available on Windows and macOS only.",
                12.0,
            );
        }
        choice(
            ui,
            ui_text(ids::SETTINGS_MEDIA_AUDIO_MAX_VOLUME),
            "settings_audio_max_volume",
            &mut audio.max_volume_percent,
            &max_volume_labels(),
        );
        ui.add_space(10.0);
        ui.label(
            RichText::new(ui_text(ids::SETTINGS_MEDIA_PASSTHROUGH_HEADING))
                .size(14.0)
                .strong()
                .color(colors.text),
        );
        let passthrough = &mut audio.passthrough;
        ui.checkbox(
            &mut passthrough.ac3,
            ui_text(ids::SETTINGS_MEDIA_PASSTHROUGH_AC3),
        );
        ui.checkbox(
            &mut passthrough.eac3,
            ui_text(ids::SETTINGS_MEDIA_PASSTHROUGH_EAC3),
        );
        ui.checkbox(
            &mut passthrough.dts,
            ui_text(ids::SETTINGS_MEDIA_PASSTHROUGH_DTS),
        );
        ui.checkbox(
            &mut passthrough.dts_hd,
            ui_text(ids::SETTINGS_MEDIA_PASSTHROUGH_DTS_HD),
        );
        ui.checkbox(
            &mut passthrough.truehd,
            ui_text(ids::SETTINGS_MEDIA_PASSTHROUGH_TRUEHD),
        );
        ui.add_space(10.0);
        sound_processing(ui, audio, colors);
    });
    finish_page(ui, app, colors, &before);
}

/// The equalizer, loudness, compression and pitch controls (M6-08, M6-09).
fn sound_processing(ui: &mut Ui, audio: &mut AudioSettings, colors: Palette) {
    ui.label(
        RichText::new(ui_text(ids::SETTINGS_MEDIA_SOUND_HEADING))
            .size(14.0)
            .strong()
            .color(colors.text),
    );
    ui.label(ui_text(ids::SETTINGS_MEDIA_EQ_HEADING));
    let mut gains = gains_from_setting(&audio.equalizer_gains_db);
    let mut changed = false;
    ui.horizontal(|ui| {
        let caption = ui.label(ui_text(ids::SETTINGS_MEDIA_EQ_PRESET));
        egui::ComboBox::from_id_salt("settings_audio_eq_preset")
            .selected_text(preset_label(current_preset(&gains)))
            .show_ui(ui, |ui| {
                for preset in EQ_PRESETS {
                    if ui
                        .selectable_label(
                            current_preset(&gains) == Some(preset),
                            preset_label(Some(preset)),
                        )
                        .clicked()
                    {
                        gains = preset_gains(preset);
                        changed = true;
                    }
                }
            })
            .response
            .labelled_by(caption.id);
    });
    ui.horizontal(|ui| {
        for (index, hz) in BAND_FREQUENCIES_HZ.iter().enumerate() {
            ui.vertical(|ui| {
                changed |= ui
                    .add(
                        egui::Slider::new(
                            &mut gains[index],
                            -EQUALIZER_GAIN_LIMIT_DB..=EQUALIZER_GAIN_LIMIT_DB,
                        )
                        .vertical()
                        .step_by(0.5)
                        .show_value(false),
                    )
                    .on_hover_text(format!("{} {:+.1} dB", band_label(*hz), gains[index]))
                    .changed();
                ui.label(
                    RichText::new(band_label(*hz))
                        .size(11.0)
                        .color(colors.muted),
                );
            });
        }
    });
    if changed {
        audio.equalizer_gains_db = gains_into_setting(&gains);
    }
    muted(ui, colors, ui_text(ids::SETTINGS_MEDIA_EQ_HELP), 12.0);
    ui.add_space(6.0);
    ui.checkbox(
        &mut audio.loudness_normalization,
        ui_text(ids::SETTINGS_MEDIA_LOUDNESS_NORMALIZE),
    );
    ui.add_enabled(
        audio.loudness_normalization,
        egui::Slider::new(&mut audio.loudness_target_lufs, LOUDNESS_TARGET_LUFS_RANGE)
            .text(ui_text(ids::SETTINGS_MEDIA_LOUDNESS_TARGET)),
    );
    ui.checkbox(
        &mut audio.dynamic_range_compression,
        ui_text(ids::SETTINGS_MEDIA_COMPRESSION),
    );
    ui.checkbox(
        &mut audio.pitch_correction,
        ui_text(ids::SETTINGS_MEDIA_PITCH_CORRECTION),
    );
}

const EQ_PRESETS: [Preset; 4] = [
    Preset::Flat,
    Preset::BassBoost,
    Preset::TrebleBoost,
    Preset::Voice,
];

fn preset_label(preset: Option<Preset>) -> &'static str {
    match preset {
        Some(Preset::Flat) => ui_text(ids::SETTINGS_MEDIA_EQ_PRESET_FLAT),
        Some(Preset::BassBoost) => ui_text(ids::SETTINGS_MEDIA_EQ_PRESET_BASS),
        Some(Preset::TrebleBoost) => ui_text(ids::SETTINGS_MEDIA_EQ_PRESET_TREBLE),
        Some(Preset::Voice) => ui_text(ids::SETTINGS_MEDIA_EQ_PRESET_VOICE),
        None => ui_text(ids::SETTINGS_MEDIA_EQ_PRESET_CUSTOM),
    }
}

/// "31 Hz", "500 Hz", "1 kHz", "16 kHz".
fn band_label(hz: u32) -> String {
    if hz >= 1_000 {
        format!("{} kHz", hz / 1_000)
    } else {
        format!("{hz} Hz")
    }
}

/// The ten band gains for the sliders: the saved gains, or all zero when none are saved.
fn gains_from_setting(saved: &[f32]) -> [f32; EQUALIZER_BANDS] {
    let mut gains = [0.0; EQUALIZER_BANDS];
    if saved.len() == EQUALIZER_BANDS {
        gains.copy_from_slice(saved);
    }
    gains
}

/// The value to save: empty when every band is flat, so no filter runs.
fn gains_into_setting(gains: &[f32; EQUALIZER_BANDS]) -> Vec<f32> {
    if gains.iter().all(|gain| *gain == 0.0) {
        Vec::new()
    } else {
        gains.to_vec()
    }
}

fn preset_gains(preset: Preset) -> [f32; EQUALIZER_BANDS] {
    preset.gains().map(|gain| gain as f32)
}

/// The preset whose gains equal `gains`, or `None` for a custom curve.
fn current_preset(gains: &[f32; EQUALIZER_BANDS]) -> Option<Preset> {
    EQ_PRESETS
        .into_iter()
        .find(|preset| preset_gains(*preset) == *gains)
}

pub(super) fn subtitles_page(ui: &mut Ui, app: &mut KattriApp, colors: Palette) {
    let before = app.settings.settings().clone();
    settings_card(ui, |ui| {
        let subtitles = &mut app.settings.settings_mut().subtitles;
        ui.horizontal(|ui| {
            ui.label(ui_text(ids::SETTINGS_MEDIA_SUBTITLE_FONT));
            ui.text_edit_singleline(&mut subtitles.font);
        });
        ui.add(
            egui::Slider::new(&mut subtitles.size, -1..=100)
                .text(ui_text(ids::SETTINGS_MEDIA_SUBTITLE_SIZE)),
        );
        ui.add(
            egui::Slider::new(&mut subtitles.border_size, -1..=10)
                .text(ui_text(ids::SETTINGS_MEDIA_SUBTITLE_OUTLINE_SIZE)),
        );
        ui.add(
            egui::Slider::new(&mut subtitles.secondary_position_percent, 0..=100)
                .text(ui_text(ids::SETTINGS_MEDIA_SUBTITLE_SECONDARY_POSITION)),
        );
        ui.checkbox(
            &mut subtitles.ass_scale_border_and_shadow,
            ui_text(ids::SETTINGS_MEDIA_SUBTITLE_ASS_SCALE),
        );
        ui.horizontal(|ui| {
            let caption = ui.label(ui_text(ids::SETTINGS_MEDIA_SUBTITLE_ASS_OVERRIDE));
            let default_label = ui_text(ids::SETTINGS_MEDIA_SUBTITLE_ASS_OVERRIDE_DEFAULT);
            let shown = subtitles
                .ass_style_override
                .as_deref()
                .filter(|value| ASS_OVERRIDES.contains(value))
                .unwrap_or(default_label)
                .to_owned();
            egui::ComboBox::from_id_salt("settings_subtitle_ass_override")
                .selected_text(shown)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut subtitles.ass_style_override, None, default_label);
                    for value in ASS_OVERRIDES {
                        ui.selectable_value(
                            &mut subtitles.ass_style_override,
                            Some((*value).to_owned()),
                            *value,
                        );
                    }
                })
                .response
                .labelled_by(caption.id);
        });
        muted(
            ui,
            colors,
            ui_text(ids::SETTINGS_MEDIA_SUBTITLE_ASS_OVERRIDE_HELP),
            12.0,
        );
    });
    finish_page(ui, app, colors, &before);
}

pub(super) fn video_page(ui: &mut Ui, app: &mut KattriApp, colors: Palette) {
    let before = app.settings.settings().clone();
    settings_card(ui, |ui| {
        let video = &mut app.settings.settings_mut().video;
        choice(
            ui,
            ui_text(ids::SETTINGS_MEDIA_VIDEO_HARDWARE_DECODE),
            "settings_video_decode",
            &mut video.decode_mode,
            &decode_modes(),
        );
        ui.checkbox(
            &mut video.deinterlace,
            ui_text(ids::SETTINGS_MEDIA_VIDEO_DEINTERLACE),
        );
        choice(
            ui,
            ui_text(ids::SETTINGS_MEDIA_VIDEO_ASPECT_RATIO),
            "settings_video_aspect",
            &mut video.aspect,
            &aspect_modes(),
        );
        choice(
            ui,
            ui_text(ids::SETTINGS_MEDIA_VIDEO_TONE_MAPPING),
            "settings_video_tone_mapping",
            &mut video.tone_mapping,
            &[
                (
                    ToneMapping::Auto,
                    ui_text(ids::SETTINGS_MEDIA_TONE_MAPPING_AUTO),
                ),
                (
                    ToneMapping::Bt2390,
                    ui_text(ids::SETTINGS_MEDIA_TONE_MAPPING_BT2390),
                ),
                (
                    ToneMapping::Mobius,
                    ui_text(ids::SETTINGS_MEDIA_TONE_MAPPING_MOBIUS),
                ),
                (
                    ToneMapping::Hable,
                    ui_text(ids::SETTINGS_MEDIA_TONE_MAPPING_HABLE),
                ),
                (
                    ToneMapping::Reinhard,
                    ui_text(ids::SETTINGS_MEDIA_TONE_MAPPING_REINHARD),
                ),
                (
                    ToneMapping::Clip,
                    ui_text(ids::SETTINGS_MEDIA_TONE_MAPPING_CLIP),
                ),
            ],
        );
        muted(
            ui,
            colors,
            ui_text(ids::SETTINGS_MEDIA_TONE_MAPPING_HELP),
            12.0,
        );
        ui.checkbox(
            &mut video.allow_zoom,
            ui_text(ids::SETTINGS_MEDIA_VIDEO_ALLOW_ZOOM),
        );
        ui.checkbox(
            &mut video.transcode.always_force,
            ui_text(ids::SETTINGS_MEDIA_VIDEO_TRANSCODE_ALL),
        );
    });
    finish_page(ui, app, colors, &before);
}

#[cfg(test)]
mod sound_tests {
    use super::*;

    #[test]
    fn saved_gains_round_trip_and_flat_is_saved_as_empty() {
        assert_eq!(gains_from_setting(&[]), [0.0; 10]);
        assert_eq!(
            gains_from_setting(&[1.0; 3]),
            [0.0; 10],
            "wrong length counts as flat"
        );
        let curve = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0];
        assert_eq!(gains_from_setting(&curve), curve);
        assert_eq!(gains_into_setting(&curve), curve.to_vec());
        assert!(gains_into_setting(&[0.0; 10]).is_empty());
    }

    #[test]
    fn every_preset_is_found_by_its_own_gains_and_a_tweak_is_custom() {
        for preset in EQ_PRESETS {
            assert_eq!(current_preset(&preset_gains(preset)), Some(preset));
        }
        let mut tweaked = preset_gains(Preset::BassBoost);
        tweaked[9] += 0.5;
        assert_eq!(current_preset(&tweaked), None);
    }

    #[test]
    fn preset_gains_stay_inside_the_setting_limit() {
        for preset in EQ_PRESETS {
            assert!(
                preset_gains(preset)
                    .iter()
                    .all(|gain| gain.abs() <= EQUALIZER_GAIN_LIMIT_DB)
            );
        }
    }

    #[test]
    fn band_labels_switch_to_kilohertz_at_one_thousand() {
        let labels: Vec<_> = BAND_FREQUENCIES_HZ
            .iter()
            .map(|hz| band_label(*hz))
            .collect();
        assert_eq!(labels[0], "31 Hz");
        assert_eq!(labels[4], "500 Hz");
        assert_eq!(labels[5], "1 kHz");
        assert_eq!(labels[9], "16 kHz");
    }
}

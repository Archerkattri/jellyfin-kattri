//! The details page's Media info section: what one media source holds, in a few lines.
//! Built from the PlaybackInfo the page already has. The file path is never shown.

use jellyfin_api::{MediaSourceInfo, MediaStreamInfo};

/// One line for the source itself, then one per stream, in server order.
pub(crate) fn summary_lines(source: &MediaSourceInfo) -> Vec<String> {
    let mut lines = Vec::new();
    let mut parts = Vec::new();
    if let Some(container) = source.container.as_deref().filter(|text| !text.is_empty()) {
        parts.push(container.to_owned());
    }
    if let Some(size) = source.size.filter(|size| *size > 0) {
        parts.push(format_size(size as u64));
    }
    if let Some(bitrate) = source.bitrate.filter(|rate| *rate > 0) {
        parts.push(format_bitrate(bitrate as u64));
    }
    if !parts.is_empty() {
        lines.push(format!("Source: {}", parts.join(" · ")));
    }
    for stream in source.media_streams.iter().flatten() {
        if let Some(line) = stream_line(stream) {
            lines.push(line);
        }
    }
    lines
}

fn stream_line(stream: &MediaStreamInfo) -> Option<String> {
    let kind = stream.stream_type.as_deref()?;
    let mut parts = Vec::new();
    let codec = stream.codec.as_deref().filter(|codec| !codec.is_empty());
    match kind {
        "Video" => {
            if let Some(codec) = codec {
                parts.push(match stream.profile.as_deref() {
                    Some(profile) if !profile.is_empty() => format!("{codec} {profile}"),
                    _ => codec.to_owned(),
                });
            }
            if let (Some(width), Some(height)) = (stream.width, stream.height) {
                parts.push(format!("{width}×{height}"));
            }
            if let Some(rate) = stream
                .real_frame_rate
                .or(stream.average_frame_rate)
                .filter(|rate| *rate > 0.0)
            {
                parts.push(format!("{} fps", trim_rate(rate)));
            }
            if let Some(depth) = stream.bit_depth.filter(|depth| *depth > 8) {
                parts.push(format!("{depth}-bit"));
            }
            if let Some(bitrate) = stream.bitrate.filter(|rate| *rate > 0) {
                parts.push(format_bitrate(bitrate as u64));
            }
        }
        "Audio" | "Subtitle" => {
            if let Some(title) = stream
                .display_title
                .as_deref()
                .filter(|title| !title.is_empty())
            {
                parts.push(title.to_owned());
            } else {
                if let Some(language) = stream.language.as_deref() {
                    parts.push(language.to_owned());
                }
                if let Some(codec) = codec {
                    parts.push(codec.to_owned());
                }
                if let Some(layout) = stream.channel_layout.as_deref() {
                    parts.push(layout.to_owned());
                }
            }
        }
        _ => return None,
    }
    if parts.is_empty() {
        return None;
    }
    Some(format!("{kind}: {}", parts.join(" · ")))
}

fn trim_rate(rate: f64) -> String {
    let text = format!("{rate:.3}");
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

fn format_size(bytes: u64) -> String {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    let bytes = bytes as f64;
    if bytes >= GIB {
        format!("{:.1} GB", bytes / GIB)
    } else {
        format!("{:.0} MB", (bytes / MIB).max(1.0))
    }
}

fn format_bitrate(bits_per_second: u64) -> String {
    if bits_per_second >= 1_000_000 {
        format!("{:.1} Mbps", bits_per_second as f64 / 1_000_000.0)
    } else {
        format!("{} kbps", (bits_per_second / 1_000).max(1))
    }
}

#[cfg(test)]
mod tests {
    use super::summary_lines;

    #[test]
    fn source_and_streams_read_as_short_lines_without_the_path() {
        let source: jellyfin_api::MediaSourceInfo = serde_json::from_value(serde_json::json!({
            "Id": "s", "Path": "/srv/media/secret/film.mkv", "Container": "mkv",
            "Size": 4_509_715_660_i64, "Bitrate": 12_300_000,
            "MediaStreams": [
                {"Type": "Video", "Codec": "hevc", "Profile": "Main 10", "Width": 3840,
                 "Height": 2160, "RealFrameRate": 23.976, "BitDepth": 10},
                {"Type": "Audio", "Codec": "eac3", "DisplayTitle": "English - Dolby Digital+ - 5.1"},
                {"Type": "Subtitle", "Codec": "subrip", "Language": "eng"},
                {"Type": "Attachment", "Codec": "ttf"}
            ]
        }))
        .expect("source fixture");
        let lines = summary_lines(&source);
        assert_eq!(
            lines,
            [
                "Source: mkv · 4.2 GB · 12.3 Mbps",
                "Video: hevc Main 10 · 3840×2160 · 23.976 fps · 10-bit",
                "Audio: English - Dolby Digital+ - 5.1",
                "Subtitle: eng · subrip",
            ]
        );
        assert!(lines.iter().all(|line| !line.contains("/srv")));
    }

    #[test]
    fn a_bare_source_has_no_lines() {
        let source: jellyfin_api::MediaSourceInfo =
            serde_json::from_value(serde_json::json!({"Id": "s"})).expect("bare source");
        assert!(summary_lines(&source).is_empty());
    }
}

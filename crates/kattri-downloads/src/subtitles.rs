//! External text subtitles as child jobs of a video download.
//!
//! Rules:
//! - Only external text subtitles. Image subtitles (PGS, VobSub, DVB) are burned in by
//!   the server and are never separate files.
//! - Language filter: when the preference list is empty, all external text subtitles
//!   are kept. Otherwise only the listed languages (case-insensitive).
//! - Each subtitle is its own `Transfer` with its own temp file. A failed subtitle is
//!   recorded as `Failed` and never fails the video.
//! - The URL is built from the item and media source IDs and the stream index. The
//!   server's `DeliveryUrl` is not used: it can carry credentials in its query.
//!   The path is `/Videos/{itemId}/{mediaSourceId}/Subtitles/{index}/Stream.{format}`.
//! - Storage: `<downloads>/<itemId>/subtitles/<index>.<format>`.

use std::path::Path;

use jellyfin_api::MediaStreamInfo;

use crate::driver::{DriverConfig, DriverError, Outcome, RequestAuth, Transfer};
use crate::fs::commit_final;
use crate::manifest::{ManifestVerification, SubtitleEntry, SubtitleStatus};
use crate::verify::Verification;

/// Codecs that are bitmap subtitles, never downloaded even if flagged as text.
const IMAGE_CODECS: &[&str] = &[
    "pgs",
    "pgssub",
    "hdmv_pgs_subtitle",
    "dvdsub",
    "vobsub",
    "xsub",
    "dvb_subtitle",
    "dvbsub",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubtitleSelection {
    pub index: i32,
    /// Lowercase container/codec name used as the file extension (for example `srt`).
    pub format: String,
    pub language: Option<String>,
}

/// Picks external text subtitles from a media source's streams.
pub fn select_external_text_subtitles(
    streams: &[MediaStreamInfo],
    languages: &[String],
) -> Vec<SubtitleSelection> {
    streams
        .iter()
        .filter_map(|stream| {
            if stream.stream_type.as_deref() != Some("Subtitle") {
                return None;
            }
            if !stream.is_external || !stream.is_text_subtitle_stream {
                return None;
            }
            let index = stream.index?;
            let format = stream.codec.as_deref()?.to_ascii_lowercase();
            if format.is_empty()
                || !format.bytes().all(|b| b.is_ascii_alphanumeric())
                || IMAGE_CODECS.contains(&format.as_str())
            {
                return None;
            }
            let language = stream.language.clone();
            if !languages.is_empty() {
                let wanted = language
                    .as_deref()
                    .is_some_and(|lang| languages.iter().any(|w| w.eq_ignore_ascii_case(lang)));
                if !wanted {
                    return None;
                }
            }
            Some(SubtitleSelection {
                index,
                format,
                language,
            })
        })
        .collect()
}

/// Builds the subtitle stream URL for one stream index of a media source.
pub fn subtitle_url(
    base: &str,
    item_id: &str,
    media_source_id: &str,
    selection: &SubtitleSelection,
) -> Result<String, DriverError> {
    let safe_id =
        |id: &str| !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if !safe_id(item_id) || !safe_id(media_source_id) {
        return Err(DriverError::InvalidItemId);
    }
    let base = base.trim_end_matches('/');
    if !(base.starts_with("http://") || base.starts_with("https://")) {
        return Err(DriverError::InvalidBaseUrl);
    }
    let index = selection.index;
    let format = &selection.format;
    Ok(format!(
        "{base}/Videos/{item_id}/{media_source_id}/Subtitles/{index}/Stream.{format}"
    ))
}

/// Result of one subtitle child job.
#[derive(Clone, Debug)]
pub struct SubtitleResult {
    pub selection: SubtitleSelection,
    pub file_name: Option<String>,
    pub verification: Option<Verification>,
    pub failed: bool,
}

impl SubtitleResult {
    pub fn to_entry(&self) -> SubtitleEntry {
        SubtitleEntry {
            index: self.selection.index,
            format: self.selection.format.clone(),
            language: self.selection.language.clone(),
            file_name: self.file_name.clone(),
            status: if self.failed {
                SubtitleStatus::Failed
            } else {
                SubtitleStatus::Committed
            },
            verification: if self.failed {
                None
            } else {
                self.verification.map(ManifestVerification::from)
            },
        }
    }
}

/// Downloads each selected subtitle as its own job. Failures are recorded per subtitle
/// and never stop the others or the parent video.
pub async fn download_subtitles(
    cfg: &DriverConfig,
    base: &str,
    item_id: &str,
    media_source_id: &str,
    selections: &[SubtitleSelection],
    auth: Option<&RequestAuth>,
    downloads_root: &Path,
) -> Vec<SubtitleResult> {
    // A subtitle has no record of its own. Sharing the video's snapshot or manifest path
    // would let a subtitle commit or failure delete the video's record.
    let mut subtitle_cfg = cfg.clone();
    subtitle_cfg.snapshot_path = None;
    subtitle_cfg.manifest_path = None;
    let mut results = Vec::with_capacity(selections.len());
    for selection in selections {
        let failed_result = |selection: &SubtitleSelection| SubtitleResult {
            selection: selection.clone(),
            file_name: None,
            verification: None,
            failed: true,
        };
        let Ok(url) = subtitle_url(base, item_id, media_source_id, selection) else {
            results.push(failed_result(selection));
            continue;
        };
        let mut transfer = Transfer::new(url);
        if transfer.run(&subtitle_cfg, auth).await != Outcome::Committed {
            results.push(failed_result(selection));
            continue;
        }
        let file_name = format!("{}.{}", selection.index, selection.format);
        let committed = transfer
            .temp_name()
            .map(|name| cfg.store.dir().join(name))
            .map(|temp| {
                let final_path = downloads_root
                    .join(item_id)
                    .join("subtitles")
                    .join(&file_name);
                commit_final(&temp, &final_path).is_ok()
            })
            .unwrap_or(false);
        results.push(SubtitleResult {
            selection: selection.clone(),
            file_name: committed.then_some(file_name),
            verification: transfer.verification(),
            failed: !committed,
        });
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(json: serde_json::Value) -> MediaStreamInfo {
        serde_json::from_value(json).expect("stream json")
    }

    fn sub(index: i32, codec: &str, lang: &str, external: bool, text: bool) -> serde_json::Value {
        serde_json::json!({
            "Type": "Subtitle",
            "Index": index,
            "Codec": codec,
            "Language": lang,
            "IsExternal": external,
            "IsTextSubtitleStream": text,
            "DeliveryUrl": "/Videos/x/y/Subtitles/1/Stream.srt?api_key=SECRET",
        })
    }

    #[test]
    fn image_and_embedded_subtitles_are_excluded() {
        let streams = vec![
            stream(sub(2, "srt", "eng", true, true)),
            stream(sub(3, "pgssub", "eng", true, false)),
            stream(sub(4, "srt", "eng", false, true)),
            stream(sub(5, "pgssub", "eng", true, true)),
        ];
        let picked = select_external_text_subtitles(&streams, &[]);
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].index, 2);
    }

    #[test]
    fn language_filter_and_empty_preference() {
        let streams = vec![
            stream(sub(2, "srt", "eng", true, true)),
            stream(sub(3, "srt", "fre", true, true)),
            stream(sub(4, "ass", "ENG", true, true)),
        ];
        let all = select_external_text_subtitles(&streams, &[]);
        assert_eq!(all.len(), 3);
        let english = select_external_text_subtitles(&streams, &["eng".to_owned()]);
        assert_eq!(
            english.iter().map(|s| s.index).collect::<Vec<_>>(),
            vec![2, 4]
        );
    }

    #[test]
    fn selection_never_reads_the_credential_bearing_delivery_url() {
        let streams = vec![stream(sub(2, "srt", "eng", true, true))];
        let picked = select_external_text_subtitles(&streams, &[]);
        let url = subtitle_url("https://srv", "item1", "item1", &picked[0]).expect("url");
        assert!(url.ends_with("/Subtitles/2/Stream.srt"));
        assert!(!url.contains("api_key"));
    }

    #[test]
    fn subtitle_url_rejects_unsafe_ids() {
        let selection = SubtitleSelection {
            index: 1,
            format: "srt".into(),
            language: None,
        };
        assert!(subtitle_url("https://srv", "a/../b", "m", &selection).is_err());
        assert!(subtitle_url("ftp://srv", "a", "m", &selection).is_err());
    }
}

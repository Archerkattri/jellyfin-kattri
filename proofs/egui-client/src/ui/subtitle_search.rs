//! Remote subtitle search dialog, modelled on Jellyfin Web's subtitle editor
//! (`src/components/subtitleeditor`): pick a language, search the server's subtitle providers,
//! and download one result. Results are grouped by provider and show the format, rating and
//! download count. The dialog only collects choices; the app sends the requests.

use egui::{Context, RichText};
use jellyfin_api::{CultureDto, RemoteSubtitleInfo};
use kattri_i18n::ids;

use super::{ui_text, ui_text_args};
use crate::worker::SubtitleRequestError;

/// Language searched first when the viewer has not picked one this session.
pub(crate) const DEFAULT_LANGUAGE: &str = "eng";

/// Outcome of the last download, shown under the results.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DownloadNotice {
    Running,
    /// Downloaded and handed to the playing video.
    AddedToPlayer,
    /// Downloaded to the server only.
    OnServer,
    Failed(SubtitleRequestError),
}

/// What the dialog holds while it is open.
#[derive(Debug)]
pub(crate) struct SubtitleSearch {
    pub item_id: String,
    pub title: String,
    /// Opened from the player, so a download is also loaded into the playing video.
    pub for_player: bool,
    pub language: String,
    pub search_request: Option<u64>,
    pub results: Option<Result<Vec<RemoteSubtitleInfo>, SubtitleRequestError>>,
    pub download_request: Option<u64>,
    pub notice: Option<DownloadNotice>,
}

impl SubtitleSearch {
    pub(crate) fn new(item_id: String, title: String, for_player: bool, language: String) -> Self {
        Self {
            item_id,
            title,
            for_player,
            language,
            search_request: None,
            results: None,
            download_request: None,
            notice: None,
        }
    }
}

/// A choice made in the dialog this frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SubtitleSearchAction {
    Search,
    Download(String),
    Close,
}

/// Results with their provider heading: the heading is set on the first result of each
/// provider, as Jellyfin Web groups them.
pub(crate) fn grouped(results: &[RemoteSubtitleInfo]) -> Vec<(Option<&str>, &RemoteSubtitleInfo)> {
    let mut last: Option<&str> = None;
    results
        .iter()
        .map(|result| {
            let provider = result
                .provider_name
                .as_deref()
                .unwrap_or_else(|| ui_text(ids::SUBTITLE_SEARCH_UNKNOWN_PROVIDER));
            let heading = (last != Some(provider)).then_some(provider);
            last = Some(provider);
            (heading, result)
        })
        .collect()
}

/// The secondary line of one result: format, rating, downloads and flags.
pub(crate) fn details_line(result: &RemoteSubtitleInfo) -> String {
    let mut parts = Vec::new();
    if let Some(format) = result.format.as_deref().filter(|f| !f.is_empty()) {
        parts.push(ui_text_args(
            ids::SUBTITLE_SEARCH_FORMAT,
            &[("format", format)],
        ));
    }
    if let Some(rating) = result
        .community_rating
        .filter(|r| r.is_finite() && *r > 0.0)
    {
        let rating = format!("{rating:.1}");
        parts.push(ui_text_args(
            ids::SUBTITLE_SEARCH_RATING,
            &[("rating", &rating)],
        ));
    }
    if let Some(count) = result.download_count {
        let count = count.to_string();
        parts.push(ui_text_args(
            ids::SUBTITLE_SEARCH_DOWNLOADS,
            &[("count", &count)],
        ));
    }
    for (flag, id) in [
        (result.is_hash_match, ids::SUBTITLE_SEARCH_HASH_MATCH),
        (
            result.hearing_impaired,
            ids::SUBTITLE_SEARCH_HEARING_IMPAIRED,
        ),
        (result.forced, ids::SUBTITLE_SEARCH_FORCED),
        (
            result.machine_translated.or(result.ai_translated),
            ids::SUBTITLE_SEARCH_MACHINE_TRANSLATED,
        ),
    ] {
        if flag == Some(true) {
            parts.push(ui_text(id).to_owned());
        }
    }
    parts.join("  ·  ")
}

fn error_text(error: SubtitleRequestError, download: bool) -> &'static str {
    match (error, download) {
        (SubtitleRequestError::Forbidden, _) => ui_text(ids::SUBTITLE_SEARCH_FORBIDDEN),
        (SubtitleRequestError::Failed, false) => ui_text(ids::SUBTITLE_SEARCH_FAILED),
        (SubtitleRequestError::Failed, true) => ui_text(ids::SUBTITLE_SEARCH_DOWNLOAD_FAILED),
    }
}

const ERROR_COLOR: egui::Color32 = egui::Color32::from_rgb(0xF0, 0xA0, 0x99);

/// Draws the dialog while `state` is `Some`. Changing the language starts a new search.
pub(crate) fn show(
    ctx: &Context,
    state: &mut SubtitleSearch,
    cultures: &[CultureDto],
) -> Option<SubtitleSearchAction> {
    let mut action = None;
    let mut open = true;
    egui::Window::new(ui_text_args(
        ids::SUBTITLE_SEARCH_TITLE,
        &[("title", state.title.as_str())],
    ))
    .id(egui::Id::unique("kattri-subtitle-search"))
    .open(&mut open)
    .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
    .collapsible(false)
    .resizable(false)
    .order(egui::Order::Foreground)
    .show(ctx, |ui| {
        ui.set_min_width(460.0);
        ui.set_max_width(560.0);
        ui.horizontal(|ui| {
            ui.label(ui_text(ids::SUBTITLE_SEARCH_LANGUAGE));
            let selected = cultures
                .iter()
                .find(|culture| culture.three_letter_code == state.language)
                .map_or(state.language.as_str(), |culture| {
                    culture.display_name.as_str()
                })
                .to_owned();
            let before = state.language.clone();
            egui::ComboBox::from_id_salt("kattri-subtitle-search-language")
                .selected_text(selected)
                .width(220.0)
                .height(320.0)
                .show_ui(ui, |ui| {
                    for culture in cultures {
                        ui.selectable_value(
                            &mut state.language,
                            culture.three_letter_code.clone(),
                            &culture.display_name,
                        );
                    }
                });
            if state.language != before {
                action = Some(SubtitleSearchAction::Search);
            }
            let searching = state.search_request.is_some();
            if ui
                .add_enabled(
                    !searching,
                    egui::Button::new(ui_text(ids::SUBTITLE_SEARCH_SEARCH)),
                )
                .clicked()
            {
                action = Some(SubtitleSearchAction::Search);
            }
        });
        ui.separator();
        if state.search_request.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(ui_text(ids::SUBTITLE_SEARCH_SEARCHING));
            });
        } else {
            match &state.results {
                Some(Ok(results)) if results.is_empty() => {
                    ui.label(ui_text(ids::SUBTITLE_SEARCH_NO_RESULTS));
                }
                Some(Ok(results)) => {
                    let downloading = state.download_request.is_some();
                    egui::ScrollArea::vertical()
                        .id_salt("kattri-subtitle-search-results")
                        .max_height(380.0)
                        .show(ui, |ui| {
                            for (heading, result) in grouped(results) {
                                if let Some(heading) = heading {
                                    ui.add_space(6.0);
                                    ui.label(RichText::new(heading).strong().size(15.0));
                                }
                                ui.horizontal(|ui| {
                                    ui.vertical(|ui| {
                                        ui.set_max_width(380.0);
                                        let name = result
                                            .name
                                            .as_deref()
                                            .filter(|name| !name.is_empty())
                                            .unwrap_or(result.id.as_str());
                                        ui.label(RichText::new(name).strong());
                                        let details = details_line(result);
                                        if !details.is_empty() {
                                            ui.label(RichText::new(details).small().weak());
                                        }
                                        if let Some(comment) =
                                            result.comment.as_deref().filter(|c| !c.is_empty())
                                        {
                                            ui.label(RichText::new(comment).small().weak());
                                        }
                                    });
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            if ui
                                                .add_enabled(
                                                    !downloading,
                                                    egui::Button::new(ui_text(
                                                        ids::SUBTITLE_SEARCH_DOWNLOAD,
                                                    )),
                                                )
                                                .clicked()
                                            {
                                                action = Some(SubtitleSearchAction::Download(
                                                    result.id.clone(),
                                                ));
                                            }
                                        },
                                    );
                                });
                                ui.add_space(4.0);
                            }
                        });
                }
                Some(Err(error)) => {
                    ui.label(RichText::new(error_text(*error, false)).color(ERROR_COLOR));
                }
                None => {}
            }
        }
        match state.notice {
            Some(DownloadNotice::Running) => {
                ui.separator();
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(ui_text(ids::SUBTITLE_SEARCH_DOWNLOADING));
                });
            }
            Some(DownloadNotice::AddedToPlayer) => {
                ui.separator();
                ui.label(ui_text(ids::SUBTITLE_SEARCH_DOWNLOADED_PLAYER));
            }
            Some(DownloadNotice::OnServer) => {
                ui.separator();
                ui.label(ui_text(ids::SUBTITLE_SEARCH_DOWNLOADED_SERVER));
            }
            Some(DownloadNotice::Failed(error)) => {
                ui.separator();
                ui.label(RichText::new(error_text(error, true)).color(ERROR_COLOR));
            }
            None => {}
        }
        ui.add_space(8.0);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button(ui_text(ids::SUBTITLE_SEARCH_CLOSE)).clicked() {
                action = Some(SubtitleSearchAction::Close);
            }
        });
    });
    if !open {
        action = Some(SubtitleSearchAction::Close);
    }
    action
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(id: &str, provider: Option<&str>) -> RemoteSubtitleInfo {
        RemoteSubtitleInfo {
            id: id.into(),
            provider_name: provider.map(str::to_owned),
            ..RemoteSubtitleInfo::default()
        }
    }

    #[test]
    fn results_get_a_heading_where_the_provider_changes() {
        let results = [
            result("a", Some("One")),
            result("b", Some("One")),
            result("c", Some("Two")),
            result("d", None),
        ];
        let headings: Vec<Option<&str>> = grouped(&results).into_iter().map(|(h, _)| h).collect();
        assert_eq!(
            headings,
            [
                Some("One"),
                None,
                Some("Two"),
                Some(ui_text(ids::SUBTITLE_SEARCH_UNKNOWN_PROVIDER))
            ]
        );
    }

    #[test]
    fn the_details_line_lists_format_rating_downloads_and_flags() {
        let mut info = result("a", Some("One"));
        info.format = Some("srt".into());
        info.community_rating = Some(8.46);
        info.download_count = Some(1200);
        info.is_hash_match = Some(true);
        info.hearing_impaired = Some(false);
        assert_eq!(
            details_line(&info),
            "Format: srt  ·  Rating: 8.5  ·  Downloads: 1200  ·  Matches this file"
        );
        assert_eq!(details_line(&result("b", None)), "");
    }
}

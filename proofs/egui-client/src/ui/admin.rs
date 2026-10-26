//! Read-only admin dashboard (M4-22): server version, library names, and the
//! active session count. It has no action and makes no write call.
//!
//! The server decides admin rights. A 403 from any read hides the section; it
//! does not show an error. The gate is `admin.dashboard` in the settings file and
//! is off by default.

use super::ui_text;
use egui::{RichText, Ui};
use jellyfin_api::{ServerInfo, VirtualFolder};
use kattri_i18n::ids;

use crate::theme::Palette;

/// The result of the three read-only calls for one load of the dashboard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AdminLoad {
    /// At least one read has not returned yet.
    Pending,
    /// The server refused a read with 403. The section stays hidden.
    Refused,
    /// A read failed for another reason, such as a network error.
    Failed,
    Loaded(AdminSnapshot),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AdminSnapshot {
    pub(crate) server_name: Option<String>,
    pub(crate) server_product: Option<String>,
    pub(crate) server_os: Option<String>,
    pub(crate) server_version: Option<String>,
    pub(crate) library_names: Vec<String>,
    pub(crate) active_sessions: usize,
}

impl AdminSnapshot {
    /// Build the snapshot from the decoded reads. A library without a usable
    /// name is still counted, so the count matches what the server returned.
    pub(crate) fn from_reads(
        server: &ServerInfo,
        libraries: &[VirtualFolder],
        active_sessions: usize,
    ) -> Self {
        let library_names = libraries
            .iter()
            .map(|library| {
                let name = library
                    .name
                    .as_deref()
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .unwrap_or(ui_text(ids::ADMIN_LIBRARY_UNNAMED))
                    .to_owned();
                let collection_type = library
                    .collection_type
                    .as_deref()
                    .map(str::trim)
                    .filter(|kind| !kind.is_empty());
                let location_count = library.locations.len();
                match (collection_type, location_count) {
                    (Some(kind), count) if count > 0 => {
                        format!("{name} · {kind} · {count} folders")
                    }
                    (Some(kind), _) => format!("{name} · {kind}"),
                    (None, count) if count > 0 => format!("{name} · {count} folders"),
                    _ => name,
                }
            })
            .collect();
        Self {
            server_name: nonempty(server.server_name.as_deref()),
            server_product: nonempty(server.product_name.as_deref()),
            server_os: nonempty(server.operating_system_display_name.as_deref()),
            server_version: server.version.clone(),
            library_names,
            active_sessions,
        }
    }
}

fn nonempty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DashboardView {
    Hidden,
    Loading,
    Unavailable,
    Rows(Vec<DashboardRow>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DashboardRow {
    pub(crate) label: &'static str,
    pub(crate) value: String,
}

fn row(label: &'static str, value: String) -> DashboardRow {
    DashboardRow { label, value }
}

fn value_for<'a>(rows: &'a [DashboardRow], label: &str) -> Option<&'a str> {
    rows.iter()
        .find(|entry| entry.label == label)
        .map(|entry| entry.value.as_str())
}

fn metric_card(ui: &mut Ui, label: &str, value: &str, colors: Palette) {
    egui::Frame::group(ui.style())
        .fill(colors.raised)
        .show(ui, |ui| {
            ui.set_min_height(70.0);
            ui.vertical(|ui| {
                ui.label(RichText::new(value).size(25.0).strong().color(colors.text));
                ui.label(RichText::new(label).size(12.0).color(colors.muted));
            });
        });
}

/// Pure mapping from the load state to what the screen draws. The gate wins
/// over the data, so a disabled dashboard never shows a section.
pub(crate) fn dashboard_view(enabled: bool, load: &AdminLoad) -> DashboardView {
    if !enabled {
        return DashboardView::Hidden;
    }
    match load {
        AdminLoad::Pending => DashboardView::Loading,
        AdminLoad::Refused => DashboardView::Hidden,
        AdminLoad::Failed => DashboardView::Unavailable,
        AdminLoad::Loaded(snapshot) => {
            let not_reported = || ui_text(ids::ADMIN_VALUE_NOT_REPORTED).to_owned();
            let mut rows = vec![
                row(
                    "Server name",
                    snapshot.server_name.clone().unwrap_or_else(&not_reported),
                ),
                row(
                    "Product",
                    snapshot
                        .server_product
                        .clone()
                        .unwrap_or_else(&not_reported),
                ),
                row(
                    "Operating system",
                    snapshot.server_os.clone().unwrap_or_else(&not_reported),
                ),
                row(
                    ui_text(ids::ADMIN_ROW_SERVER_VERSION),
                    snapshot.server_version.clone().unwrap_or_else(not_reported),
                ),
                row(
                    ui_text(ids::ADMIN_ROW_LIBRARIES),
                    snapshot.library_names.len().to_string(),
                ),
            ];
            rows.extend(
                snapshot
                    .library_names
                    .iter()
                    .map(|name| row(ui_text(ids::ADMIN_ROW_LIBRARY), name.clone())),
            );
            rows.push(row(
                ui_text(ids::ADMIN_ROW_ACTIVE_SESSIONS),
                snapshot.active_sessions.to_string(),
            ));
            DashboardView::Rows(rows)
        }
    }
}

pub(crate) fn show(ui: &mut Ui, view: &DashboardView, colors: Palette) {
    match view {
        DashboardView::Hidden => {}
        DashboardView::Loading => {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.label(RichText::new(ui_text(ids::ADMIN_STATUS_LOADING)).color(colors.muted));
            });
        }
        DashboardView::Unavailable => {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.label(RichText::new(ui_text(ids::ADMIN_ERROR_LOAD_FAILED)).color(colors.muted));
            });
        }
        DashboardView::Rows(rows) => {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.label(
                    RichText::new(ui_text(ids::ADMIN_HEADING_TITLE))
                        .size(22.0)
                        .strong()
                        .color(colors.text),
                );
                ui.label(
                    RichText::new("Read-only overview of this Jellyfin server.")
                        .size(13.0)
                        .color(colors.muted),
                );
                ui.add_space(14.0);

                let library_count_label = ui_text(ids::ADMIN_ROW_LIBRARIES);
                let sessions_label = ui_text(ids::ADMIN_ROW_ACTIVE_SESSIONS);
                let library_count = value_for(rows, library_count_label).unwrap_or("0");
                let active_sessions = value_for(rows, sessions_label).unwrap_or("0");
                if ui.available_width() >= 420.0 {
                    ui.columns(2, |columns| {
                        metric_card(&mut columns[0], library_count_label, library_count, colors);
                        metric_card(&mut columns[1], sessions_label, active_sessions, colors);
                    });
                } else {
                    ui.vertical(|ui| {
                        metric_card(ui, library_count_label, library_count, colors);
                        ui.add_space(6.0);
                        metric_card(ui, sessions_label, active_sessions, colors);
                    });
                }

                ui.add_space(14.0);
                ui.label(
                    RichText::new("SERVER")
                        .size(11.0)
                        .strong()
                        .color(colors.muted),
                );
                egui::Frame::group(ui.style())
                    .fill(colors.panel)
                    .show(ui, |ui| {
                        egui::Grid::new("admin_server_identity")
                            .num_columns(2)
                            .spacing([18.0, 6.0])
                            .show(ui, |ui| {
                                for (label, fallback) in [
                                    ("Server name", ui_text(ids::ADMIN_VALUE_NOT_REPORTED)),
                                    ("Product", ui_text(ids::ADMIN_VALUE_NOT_REPORTED)),
                                    ("Operating system", ui_text(ids::ADMIN_VALUE_NOT_REPORTED)),
                                    (
                                        ui_text(ids::ADMIN_ROW_SERVER_VERSION),
                                        ui_text(ids::ADMIN_VALUE_NOT_REPORTED),
                                    ),
                                ] {
                                    if let Some(value) = value_for(rows, label) {
                                        ui.label(RichText::new(label).color(colors.muted));
                                        ui.label(
                                            RichText::new(value)
                                                .color(colors.text)
                                                .text_style(egui::TextStyle::Body),
                                        );
                                    } else {
                                        ui.label(RichText::new(label).color(colors.muted));
                                        ui.label(RichText::new(fallback).color(colors.text));
                                    }
                                    ui.end_row();
                                }
                            });
                    });

                let libraries: Vec<&str> = rows
                    .iter()
                    .filter(|entry| entry.label == ui_text(ids::ADMIN_ROW_LIBRARY))
                    .map(|entry| entry.value.as_str())
                    .collect();
                ui.add_space(14.0);
                ui.label(
                    RichText::new("LIBRARIES")
                        .size(11.0)
                        .strong()
                        .color(colors.muted),
                );
                if libraries.is_empty() {
                    ui.label(RichText::new("No libraries reported").color(colors.muted));
                } else {
                    for name in libraries {
                        egui::Frame::group(ui.style())
                            .fill(colors.panel)
                            .show(ui, |ui| {
                                ui.label(RichText::new(name).color(colors.text));
                            });
                        ui.add_space(4.0);
                    }
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_read_shows_the_unavailable_message_only_when_enabled() {
        assert_eq!(
            dashboard_view(true, &AdminLoad::Failed),
            DashboardView::Unavailable
        );
        assert_eq!(
            dashboard_view(false, &AdminLoad::Failed),
            DashboardView::Hidden
        );
    }

    fn server_info() -> ServerInfo {
        ServerInfo {
            server_name: Some("Mock Jellyfin".to_owned()),
            product_name: Some("Jellyfin Server".to_owned()),
            operating_system_display_name: Some("Linux".to_owned()),
            version: Some("0.0.0-mock".to_owned()),
            ..ServerInfo::default()
        }
    }

    fn library(name: Option<&str>) -> VirtualFolder {
        VirtualFolder {
            name: name.map(str::to_owned),
            ..VirtualFolder::default()
        }
    }

    fn library_details(name: &str, collection_type: &str, locations: &[&str]) -> VirtualFolder {
        VirtualFolder {
            name: Some(name.to_owned()),
            collection_type: Some(collection_type.to_owned()),
            locations: locations.iter().map(|path| (*path).to_owned()).collect(),
            ..VirtualFolder::default()
        }
    }

    /// The fixture shape from `mock-jellyfin`: three libraries, one active session.
    fn mock_snapshot() -> AdminSnapshot {
        let libraries = [
            library(Some("Movies")),
            library(Some("Shows")),
            library(Some("Music")),
        ];
        AdminSnapshot::from_reads(&server_info(), &libraries, 1)
    }

    #[test]
    fn empty_library_list_shows_zero_libraries_and_no_name_rows() {
        let snapshot = AdminSnapshot::from_reads(&server_info(), &[], 0);
        let view = dashboard_view(true, &AdminLoad::Loaded(snapshot));
        assert_eq!(
            view,
            DashboardView::Rows(vec![
                row("Server name", "Mock Jellyfin".to_owned()),
                row("Product", "Jellyfin Server".to_owned()),
                row("Operating system", "Linux".to_owned()),
                row("Server version", "0.0.0-mock".to_owned()),
                row("Libraries", "0".to_owned()),
                row("Active sessions", "0".to_owned()),
            ])
        );
    }

    #[test]
    fn filled_data_lists_each_library_name_in_server_order() {
        let view = dashboard_view(true, &AdminLoad::Loaded(mock_snapshot()));
        assert_eq!(
            view,
            DashboardView::Rows(vec![
                row("Server name", "Mock Jellyfin".to_owned()),
                row("Product", "Jellyfin Server".to_owned()),
                row("Operating system", "Linux".to_owned()),
                row("Server version", "0.0.0-mock".to_owned()),
                row("Libraries", "3".to_owned()),
                row("Library", "Movies".to_owned()),
                row("Library", "Shows".to_owned()),
                row("Library", "Music".to_owned()),
                row("Active sessions", "1".to_owned()),
            ])
        );
    }

    #[test]
    fn unnamed_and_blank_libraries_are_counted_with_a_fallback_name() {
        let libraries = [
            library(None),
            library(Some("   ")),
            library(Some(" Photos ")),
        ];
        let snapshot = AdminSnapshot::from_reads(&server_info(), &libraries, 2);
        assert_eq!(
            snapshot.library_names,
            vec![
                ui_text(ids::ADMIN_LIBRARY_UNNAMED).to_owned(),
                ui_text(ids::ADMIN_LIBRARY_UNNAMED).to_owned(),
                "Photos".to_owned(),
            ]
        );
        assert_eq!(snapshot.active_sessions, 2);
    }

    #[test]
    fn library_rows_include_available_type_and_location_count_without_paths() {
        let libraries = [
            library_details("Movies", "movies", &["/media/films", "/archive/films"]),
            library_details("Music", "music", &[]),
        ];
        let snapshot = AdminSnapshot::from_reads(&server_info(), &libraries, 0);
        assert_eq!(
            snapshot.library_names,
            vec!["Movies · movies · 2 folders", "Music · music"]
        );
    }

    #[test]
    fn missing_version_is_shown_as_not_reported() {
        let snapshot = AdminSnapshot::from_reads(&ServerInfo::default(), &[], 0);
        let view = dashboard_view(true, &AdminLoad::Loaded(snapshot));
        let DashboardView::Rows(rows) = view else {
            panic!("expected rows");
        };
        assert_eq!(
            rows[3],
            row(
                "Server version",
                ui_text(ids::ADMIN_VALUE_NOT_REPORTED).to_owned()
            )
        );
    }

    #[test]
    fn missing_server_identity_fields_use_the_not_reported_fallback() {
        let view = dashboard_view(
            true,
            &AdminLoad::Loaded(AdminSnapshot::from_reads(&ServerInfo::default(), &[], 0)),
        );
        let DashboardView::Rows(rows) = view else {
            panic!("expected rows");
        };
        let fallback = ui_text(ids::ADMIN_VALUE_NOT_REPORTED).to_owned();
        assert_eq!(rows[0], row("Server name", fallback.clone()));
        assert_eq!(rows[1], row("Product", fallback.clone()));
        assert_eq!(rows[2], row("Operating system", fallback));
    }

    #[test]
    fn blank_server_identity_fields_use_the_not_reported_fallback() {
        let server = ServerInfo {
            server_name: Some("  ".to_owned()),
            product_name: Some(" Jellyfin ".to_owned()),
            operating_system_display_name: Some("\t".to_owned()),
            ..ServerInfo::default()
        };
        let view = dashboard_view(
            true,
            &AdminLoad::Loaded(AdminSnapshot::from_reads(&server, &[], 0)),
        );
        let DashboardView::Rows(rows) = view else {
            panic!("expected rows");
        };
        assert_eq!(
            rows[0],
            row(
                "Server name",
                ui_text(ids::ADMIN_VALUE_NOT_REPORTED).to_owned()
            )
        );
        assert_eq!(rows[1], row("Product", "Jellyfin".to_owned()));
        assert_eq!(
            rows[2],
            row(
                "Operating system",
                ui_text(ids::ADMIN_VALUE_NOT_REPORTED).to_owned()
            )
        );
    }

    #[test]
    fn gate_off_by_default_hides_the_section_even_with_data() {
        let view = dashboard_view(false, &AdminLoad::Loaded(mock_snapshot()));
        assert_eq!(view, DashboardView::Hidden);
        assert_eq!(
            dashboard_view(false, &AdminLoad::Pending),
            DashboardView::Hidden
        );
    }

    #[test]
    fn forbidden_read_hides_the_section_when_the_gate_is_on() {
        assert_eq!(
            dashboard_view(true, &AdminLoad::Refused),
            DashboardView::Hidden
        );
    }

    #[test]
    fn pending_read_shows_loading_when_the_gate_is_on() {
        assert_eq!(
            dashboard_view(true, &AdminLoad::Pending),
            DashboardView::Loading
        );
    }
}

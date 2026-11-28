//! Administration dashboard and session-scoped user-management state. The
//! overview remains read-only; user writes are emitted as typed intents by the
//! separate `admin_users` component and authorized again in the worker.

use super::{ui_text, ui_text_args};
use egui::{RichText, Ui};
use jellyfin_api::{
    ActivityLogEntry, AdminUser, AdminUserPolicyChanges, ErrorClass, ScheduledTask, ServerInfo,
    SessionInfo, UserCounts, VirtualFolder,
};
use kattri_i18n::ids;

use crate::theme::Palette;
use crate::ui::admin_users::AdminUsersState;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AdminUserMutationKind {
    Create,
    Rename,
    PolicyPatch,
    Delete,
}

/// Non-secret details kept only while one mutation is in flight.
pub(crate) enum PendingAdminUserMutation {
    Create,
    Rename {
        user_id: String,
        name: String,
    },
    PolicyPatch {
        user_id: String,
        changes: AdminUserPolicyChanges,
    },
    Delete {
        user_id: String,
    },
}

impl PendingAdminUserMutation {
    pub(crate) fn kind(&self) -> AdminUserMutationKind {
        match self {
            Self::Create => AdminUserMutationKind::Create,
            Self::Rename { .. } => AdminUserMutationKind::Rename,
            Self::PolicyPatch { .. } => AdminUserMutationKind::PolicyPatch,
            Self::Delete { .. } => AdminUserMutationKind::Delete,
        }
    }
}

enum AdminUsersInFlight {
    List {
        request_id: u64,
    },
    Mutation {
        request_id: u64,
        mutation: PendingAdminUserMutation,
    },
}

/// Session-scoped data and correlation state for the explicit user-management view.
#[derive(Default)]
pub(crate) struct AdminUsersData {
    pub(crate) users: Vec<AdminUser>,
    pub(crate) error: Option<String>,
    pub(crate) component: AdminUsersState,
    generation: u64,
    in_flight: Option<AdminUsersInFlight>,
}

impl AdminUsersData {
    pub(crate) fn is_loading(&self) -> bool {
        self.in_flight.is_some()
    }

    pub(crate) fn pending_policy_revokes_admin(&self, current_user_id: &str) -> bool {
        let Some(AdminUsersInFlight::Mutation {
            mutation:
                PendingAdminUserMutation::PolicyPatch {
                    user_id, changes, ..
                },
            ..
        }) = self.in_flight.as_ref()
        else {
            return false;
        };
        same_admin_user_id(user_id, current_user_id)
            && (changes.is_administrator == Some(false) || changes.is_disabled == Some(true))
    }

    pub(crate) fn reset(&mut self, generation: u64) {
        self.component.cancel();
        self.users.clear();
        self.error = None;
        self.generation = generation;
        self.in_flight = None;
    }

    pub(crate) fn begin_list(&mut self, request_id: u64, generation: u64) -> bool {
        if generation != self.generation || self.is_loading() {
            return false;
        }
        self.error = None;
        self.in_flight = Some(AdminUsersInFlight::List { request_id });
        true
    }

    pub(crate) fn finish_list(
        &mut self,
        request_id: u64,
        generation: u64,
        result: Result<Vec<AdminUser>, ErrorClass>,
    ) -> bool {
        if generation != self.generation
            || !matches!(
                &self.in_flight,
                Some(AdminUsersInFlight::List { request_id: pending }) if *pending == request_id
            )
        {
            return false;
        }
        self.in_flight = None;
        match result {
            Ok(users) => {
                self.users = users;
                self.error = None;
            }
            Err(error) => self.error = Some(admin_users_error(error).to_owned()),
        }
        true
    }

    pub(crate) fn begin_mutation(
        &mut self,
        request_id: u64,
        generation: u64,
        mutation: PendingAdminUserMutation,
    ) -> bool {
        if generation != self.generation || self.is_loading() {
            return false;
        }
        self.error = None;
        self.in_flight = Some(AdminUsersInFlight::Mutation {
            request_id,
            mutation,
        });
        true
    }

    pub(crate) fn finish_mutation(
        &mut self,
        request_id: u64,
        generation: u64,
        kind: AdminUserMutationKind,
        result: Result<Option<AdminUser>, ErrorClass>,
    ) -> bool {
        if generation != self.generation {
            return false;
        }
        let Some(AdminUsersInFlight::Mutation {
            request_id: pending_id,
            mutation,
        }) = self.in_flight.take()
        else {
            return false;
        };
        if pending_id != request_id || mutation.kind() != kind {
            self.in_flight = Some(AdminUsersInFlight::Mutation {
                request_id: pending_id,
                mutation,
            });
            return false;
        }

        match result {
            Err(error) => self.error = Some(admin_users_error(error).to_owned()),
            Ok(created) => {
                self.error = None;
                match mutation {
                    PendingAdminUserMutation::Create => {
                        if let Some(user) = created {
                            self.users.push(user);
                        } else {
                            self.error = Some(
                                "User created, but refresh is needed to show the account."
                                    .to_owned(),
                            );
                        }
                    }
                    PendingAdminUserMutation::Rename { user_id, name } => {
                        if let Some(user) = self.users.iter_mut().find(|user| user.id == user_id) {
                            user.name = name;
                        }
                    }
                    PendingAdminUserMutation::PolicyPatch { user_id, changes } => {
                        if let Some(policy) = self
                            .users
                            .iter_mut()
                            .find(|user| user.id == user_id)
                            .and_then(|user| user.policy.as_mut())
                        {
                            apply_policy_changes(policy, &changes);
                        }
                    }
                    PendingAdminUserMutation::Delete { user_id } => {
                        self.users.retain(|user| user.id != user_id);
                    }
                }
            }
        }
        true
    }
}

fn same_admin_user_id(left: &str, right: &str) -> bool {
    left.replace('-', "")
        .eq_ignore_ascii_case(&right.replace('-', ""))
}

fn apply_policy_changes(
    policy: &mut jellyfin_api::AdminUserPolicy,
    changes: &AdminUserPolicyChanges,
) {
    if let Some(value) = changes.is_administrator {
        policy.is_administrator = Some(value);
    }
    if let Some(value) = changes.is_hidden {
        policy.is_hidden = Some(value);
    }
    if let Some(value) = changes.is_disabled {
        policy.is_disabled = Some(value);
    }
    if let Some(value) = changes.enable_media_playback {
        policy.enable_media_playback = Some(value);
    }
    if let Some(value) = changes.enable_remote_access {
        policy.enable_remote_access = Some(value);
    }
}

fn admin_users_error(error: ErrorClass) -> &'static str {
    match error {
        ErrorClass::Offline => "The server is unreachable. Check the connection and retry.",
        ErrorClass::Timeout => "The server took too long to respond. Please retry.",
        ErrorClass::ServerUnavailable => "The server is unavailable right now. Please retry.",
        ErrorClass::AuthExpired => "Your sign-in has expired. Sign in again.",
        ErrorClass::NotFound => "The selected user is no longer available. Refresh the list.",
        ErrorClass::Forbidden => "The server did not allow this user-management action.",
        ErrorClass::RefusedPlaintext => "The connection is not allowed because it is not secure.",
        ErrorClass::TlsFailure => "The server's secure connection could not be verified.",
        ErrorClass::Unexpected => "The user-management request failed. Please retry.",
    }
}

/// Detail rows drawn for scheduled tasks that need attention. The summary row
/// still counts every task, so a truncated list never hides a total.
const MAX_TASK_DETAIL_ROWS: usize = 8;

/// The result of the read-only calls for one load of the dashboard.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum AdminLoad {
    /// At least one read has not returned yet.
    Pending,
    /// The server refused a read with 403. The section stays hidden.
    Refused,
    /// A read failed for another reason, such as a network error.
    Failed,
    Loaded(Box<AdminSnapshot>),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AdminSnapshot {
    pub(crate) server_name: Option<String>,
    pub(crate) server_product: Option<String>,
    pub(crate) server_os: Option<String>,
    pub(crate) server_version: Option<String>,
    pub(crate) library_names: Vec<String>,
    /// Item counts in library order. `None` means that count was not reported.
    pub(crate) library_item_counts: Vec<Option<u32>>,
    pub(crate) active_sessions: usize,
    pub(crate) users: UserCounts,
    pub(crate) tasks: Vec<ScheduledTask>,
    pub item_counts: Option<jellyfin_api::ItemCounts>,
    pub active_sessions_list: Vec<SessionInfo>,
    pub activity_entries: Vec<ActivityLogEntry>,
}

impl AdminSnapshot {
    /// Build the snapshot from the decoded reads. A library without a usable
    /// name is still counted, so the count matches what the server returned.
    /// `library_item_counts` is read positionally; a missing entry means the
    /// count was not available for that library.
    #[allow(clippy::too_many_arguments)] // Mirrors the independent dashboard reads without inventing a wrapper struct.
    pub(crate) fn from_reads(
        server: &ServerInfo,
        libraries: &[VirtualFolder],
        library_item_counts: &[Option<u32>],
        active_sessions: usize,
        users: UserCounts,
        tasks: &[ScheduledTask],
        item_counts: Option<jellyfin_api::ItemCounts>,
        active_sessions_list: Vec<SessionInfo>,
        activity_entries: Vec<ActivityLogEntry>,
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
        let library_item_counts = libraries
            .iter()
            .enumerate()
            .map(|(index, _)| library_item_counts.get(index).copied().flatten())
            .collect();
        Self {
            server_name: nonempty(server.server_name.as_deref()),
            server_product: nonempty(server.product_name.as_deref()),
            server_os: nonempty(server.operating_system_display_name.as_deref()),
            server_version: server.version.clone(),
            library_names,
            library_item_counts,
            active_sessions,
            users,
            tasks: tasks
                .iter()
                .take(jellyfin_api::MAX_SCHEDULED_TASKS)
                .cloned()
                .collect(),
            item_counts,
            active_sessions_list,
            activity_entries,
        }
    }
}

fn nonempty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum DashboardView {
    Hidden,
    Loading,
    Unavailable,
    Rows {
        rows: Vec<DashboardRow>,
        sessions: Vec<SessionInfo>,
        activity: Vec<ActivityLogEntry>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DashboardRow {
    pub(crate) label: &'static str,
    pub(crate) value: String,
    pub(crate) task_id: Option<String>,
}

fn row(label: &'static str, value: String) -> DashboardRow {
    DashboardRow {
        label,
        value,
        task_id: None,
    }
}

fn value_for<'a>(rows: &'a [DashboardRow], label: &str) -> Option<&'a str> {
    rows.iter()
        .find(|entry| entry.label == label)
        .map(|entry| entry.value.as_str())
}

fn metric_card(ui: &mut Ui, label: &str, value: &str, colors: Palette) {
    egui::Frame::group(ui.style())
        .fill(colors.raised)
        .stroke(egui::Stroke::new(1.0, egui::Color32::from_white_alpha(24)))
        .inner_margin(egui::Margin::symmetric(18, 14))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.set_min_height(70.0);
            ui.vertical(|ui| {
                ui.label(RichText::new(value).size(28.0).strong().color(colors.text));
                ui.label(RichText::new(label).size(12.0).color(colors.muted));
            });
        });
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ServerPowerConfirm {
    Restart,
    Shutdown,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum ActivityFilter {
    #[default]
    All,
    User,
    System,
}

fn show_server_section(
    ui: &mut Ui,
    rows: &[DashboardRow],
    colors: Palette,
    action: &mut AdminAction,
) {
    let confirm_id = egui::Id::unique("admin_server_power_confirm");
    let mut confirm = ui
        .ctx()
        .data(|data| data.get_temp::<ServerPowerConfirm>(confirm_id));
    let prev_confirm = confirm;

    ui.horizontal(|ui| {
        ui.label(
            RichText::new("SERVER")
                .size(11.0)
                .strong()
                .color(colors.muted),
        );
        ui.with_layout(
            egui::Layout::right_to_left(egui::Align::Center),
            |ui| match confirm {
                Some(ServerPowerConfirm::Restart) => {
                    if ui.button("Cancel").clicked() {
                        confirm = None;
                    }
                    if ui
                        .button(RichText::new("Confirm Restart").color(colors.danger))
                        .clicked()
                    {
                        action.restart_server = true;
                        confirm = None;
                    }
                }
                Some(ServerPowerConfirm::Shutdown) => {
                    if ui.button("Cancel").clicked() {
                        confirm = None;
                    }
                    if ui
                        .button(RichText::new("Confirm Shutdown").color(colors.danger))
                        .clicked()
                    {
                        action.shutdown_server = true;
                        confirm = None;
                    }
                }
                None => {
                    if ui.button("Shutdown").clicked() {
                        confirm = Some(ServerPowerConfirm::Shutdown);
                    }
                    if ui.button("Restart").clicked() {
                        confirm = Some(ServerPowerConfirm::Restart);
                    }
                }
            },
        );
    });

    if confirm != prev_confirm {
        ui.ctx().data_mut(|data| match confirm {
            Some(state) => {
                data.insert_temp(confirm_id, state);
            }
            None => data.remove::<ServerPowerConfirm>(confirm_id),
        });
    }

    egui::Frame::group(ui.style())
        .fill(colors.panel)
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
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
}

fn show_libraries_section(ui: &mut Ui, libraries: &[&str], colors: Palette) {
    ui.horizontal(|ui| {
        ui.set_min_height(22.0);
        ui.label(
            RichText::new("LIBRARIES")
                .size(11.0)
                .strong()
                .color(colors.muted),
        );
    });
    if libraries.is_empty() {
        egui::Frame::group(ui.style())
            .fill(colors.panel)
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.label(RichText::new("No libraries reported").color(colors.muted));
            });
    } else {
        for name in libraries {
            egui::Frame::group(ui.style())
                .fill(colors.panel)
                .show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.label(RichText::new(*name).color(colors.text));
                });
            ui.add_space(4.0);
        }
    }
}

fn task_text(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

/// A task is running when the server reports any state other than `Idle`.
fn task_is_running(task: &ScheduledTask) -> bool {
    task_text(task.state.as_deref()).is_some_and(|state| !state.eq_ignore_ascii_case("Idle"))
}

/// `Completed` and `Aborted` are normal outcomes. Anything else the server
/// reports as the last status is worth an administrator's attention.
fn task_last_failed(task: &ScheduledTask) -> bool {
    task_text(task.last_status.as_deref()).is_some_and(|status| {
        !status.eq_ignore_ascii_case("Completed") && !status.eq_ignore_ascii_case("Aborted")
    })
}

fn task_needs_attention(task: &ScheduledTask) -> bool {
    task_is_running(task) || task_last_failed(task)
}

/// The summary counts every task, so the capped detail rows never hide a total.
fn scheduled_task_summary(snapshot: &AdminSnapshot) -> String {
    let running = snapshot
        .tasks
        .iter()
        .filter(|task| task_is_running(task))
        .count();
    let failed = snapshot
        .tasks
        .iter()
        .filter(|task| task_last_failed(task))
        .count();
    let mut summary = snapshot.tasks.len().to_string();
    if running > 0 {
        summary.push_str(&format!(" · {running} running"));
    }
    if failed > 0 {
        summary.push_str(&format!(" · {failed} last failed"));
    }
    summary
}

fn task_line(task: &ScheduledTask) -> String {
    let name = task_text(task.name.as_deref()).unwrap_or(ui_text(ids::ADMIN_VALUE_NOT_REPORTED));
    let state = task_text(task.state.as_deref()).unwrap_or(ui_text(ids::ADMIN_VALUE_NOT_REPORTED));
    let last = task_text(task.last_status.as_deref()).unwrap_or(ui_text(ids::ADMIN_TASK_NEVER_RUN));
    if task_is_running(task)
        && let Some(progress) = task.current_progress_percentage
    {
        format!("{name} · {state} ({progress:.0}%) · {last}")
    } else {
        format!("{name} · {state} · {last}")
    }
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
                    .enumerate()
                    .map(|(index, name)| {
                        let items = snapshot
                            .library_item_counts
                            .get(index)
                            .copied()
                            .flatten()
                            .map(|count| format!(" · {count} items"));
                        row(
                            ui_text(ids::ADMIN_ROW_LIBRARY),
                            format!("{name}{}", items.unwrap_or_default()),
                        )
                    }),
            );
            rows.push(row(
                ui_text(ids::ADMIN_ROW_USERS),
                match snapshot.users.disabled {
                    0 => snapshot.users.total.to_string(),
                    disabled => format!("{} · {disabled} disabled", snapshot.users.total),
                },
            ));
            rows.push(row(
                ui_text(ids::ADMIN_ROW_ACTIVE_SESSIONS),
                snapshot.active_sessions.to_string(),
            ));
            if let Some(counts) = snapshot.item_counts {
                rows.push(row("Movies", counts.movie_count.unwrap_or(0).to_string()));
                rows.push(row("Series", counts.series_count.unwrap_or(0).to_string()));
                rows.push(row(
                    "Episodes",
                    counts.episode_count.unwrap_or(0).to_string(),
                ));
                rows.push(row(
                    "Collections",
                    counts.box_set_count.unwrap_or(0).to_string(),
                ));
            }
            rows.push(row(
                ui_text(ids::ADMIN_ROW_SCHEDULED_TASKS),
                scheduled_task_summary(snapshot),
            ));
            let attention: Vec<&ScheduledTask> = snapshot
                .tasks
                .iter()
                .filter(|task| task_needs_attention(task))
                .collect();
            rows.extend(
                attention
                    .iter()
                    .take(MAX_TASK_DETAIL_ROWS)
                    .map(|task| DashboardRow {
                        label: ui_text(ids::ADMIN_ROW_TASK),
                        value: task_line(task),
                        task_id: task.id.clone(),
                    }),
            );
            if attention.len() > MAX_TASK_DETAIL_ROWS {
                rows.push(row(
                    ui_text(ids::ADMIN_ROW_TASK),
                    ui_text_args(
                        ids::ADMIN_TASK_MORE,
                        &[(
                            "count",
                            &(attention.len() - MAX_TASK_DETAIL_ROWS).to_string(),
                        )],
                    ),
                ));
            }
            DashboardView::Rows {
                rows,
                sessions: snapshot.active_sessions_list.clone(),
                activity: snapshot.activity_entries.clone(),
            }
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct AdminAction {
    pub manage_users: bool,
    pub refresh_dashboard: bool,
    pub scan_libraries: bool,
    pub run_task_id: Option<String>,
    pub restart_server: bool,
    pub shutdown_server: bool,
    pub send_session_message: Option<(String, String)>,
    pub revoke_device_id: Option<String>,
}

fn show_header_actions(ui: &mut Ui, can_manage_users: bool, action: &mut AdminAction) {
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        if can_manage_users && ui.button("Manage users").clicked() {
            action.manage_users = true;
        }
        if ui.button("Scan All Libraries").clicked() {
            action.scan_libraries = true;
        }
        if ui.button("Refresh").clicked() {
            action.refresh_dashboard = true;
        }
    });
}

pub(crate) fn show(
    ui: &mut Ui,
    view: &DashboardView,
    colors: Palette,
    can_manage_users: bool,
) -> AdminAction {
    let mut action = AdminAction::default();
    match view {
        DashboardView::Hidden => {}
        DashboardView::Loading => {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(ui_text(ids::ADMIN_STATUS_LOADING)).color(colors.muted));
                    show_header_actions(ui, can_manage_users, &mut action);
                });
            });
        }
        DashboardView::Unavailable => {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(ui_text(ids::ADMIN_ERROR_LOAD_FAILED)).color(colors.muted),
                    );
                    show_header_actions(ui, can_manage_users, &mut action);
                });
            });
        }
        DashboardView::Rows {
            rows,
            sessions,
            activity,
        } => {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(ui_text(ids::ADMIN_HEADING_TITLE))
                            .size(22.0)
                            .strong()
                            .color(colors.text),
                    );
                    show_header_actions(ui, can_manage_users, &mut action);
                });
                ui.add_space(14.0);

                let library_count_label = ui_text(ids::ADMIN_ROW_LIBRARIES);
                let users_label = ui_text(ids::ADMIN_ROW_USERS);
                let sessions_label = ui_text(ids::ADMIN_ROW_ACTIVE_SESSIONS);
                let library_count = value_for(rows, library_count_label).unwrap_or("0");
                let users = value_for(rows, users_label).unwrap_or("0");
                let active_sessions = value_for(rows, sessions_label).unwrap_or("0");
                if ui.available_width() >= 620.0 {
                    ui.columns(3, |columns| {
                        metric_card(&mut columns[0], library_count_label, library_count, colors);
                        metric_card(&mut columns[1], users_label, users, colors);
                        metric_card(&mut columns[2], sessions_label, active_sessions, colors);
                    });
                } else {
                    ui.vertical(|ui| {
                        metric_card(ui, library_count_label, library_count, colors);
                        ui.add_space(6.0);
                        metric_card(ui, users_label, users, colors);
                        ui.add_space(6.0);
                        metric_card(ui, sessions_label, active_sessions, colors);
                    });
                }

                if let (Some(movies), Some(series), Some(episodes), Some(collections)) = (
                    value_for(rows, "Movies"),
                    value_for(rows, "Series"),
                    value_for(rows, "Episodes"),
                    value_for(rows, "Collections"),
                ) {
                    ui.add_space(8.0);
                    let cards = [
                        ("Movies", movies),
                        ("Series", series),
                        ("Episodes", episodes),
                        ("Collections", collections),
                    ];
                    if ui.available_width() >= 620.0 {
                        ui.columns(4, |columns| {
                            for (index, (label, value)) in cards.iter().enumerate() {
                                metric_card(&mut columns[index], label, value, colors);
                            }
                        });
                    } else {
                        ui.columns(2, |columns| {
                            metric_card(&mut columns[0], cards[0].0, cards[0].1, colors);
                            metric_card(&mut columns[1], cards[1].0, cards[1].1, colors);
                        });
                        ui.add_space(6.0);
                        ui.columns(2, |columns| {
                            metric_card(&mut columns[0], cards[2].0, cards[2].1, colors);
                            metric_card(&mut columns[1], cards[3].0, cards[3].1, colors);
                        });
                    }
                }

                let libraries: Vec<&str> = rows
                    .iter()
                    .filter(|entry| entry.label == ui_text(ids::ADMIN_ROW_LIBRARY))
                    .map(|entry| entry.value.as_str())
                    .collect();

                ui.add_space(14.0);
                if ui.available_width() >= 720.0 {
                    ui.columns(2, |columns| {
                        show_server_section(&mut columns[0], rows, colors, &mut action);
                        show_libraries_section(&mut columns[1], &libraries, colors);
                    });
                } else {
                    ui.vertical(|ui| {
                        show_server_section(ui, rows, colors, &mut action);
                        ui.add_space(14.0);
                        show_libraries_section(ui, &libraries, colors);
                    });
                }

                let task_rows: Vec<&DashboardRow> = rows
                    .iter()
                    .filter(|entry| entry.label == ui_text(ids::ADMIN_ROW_TASK))
                    .collect();
                let task_summary =
                    value_for(rows, ui_text(ids::ADMIN_ROW_SCHEDULED_TASKS)).unwrap_or("0");
                ui.add_space(14.0);
                ui.label(
                    RichText::new("SCHEDULED TASKS")
                        .size(11.0)
                        .strong()
                        .color(colors.muted),
                );
                egui::Frame::group(ui.style())
                    .fill(colors.panel)
                    .show(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        ui.label(RichText::new(task_summary).color(colors.text));
                        for entry in task_rows {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(&entry.value).color(colors.muted));
                                if let Some(task_id) = entry.task_id.as_ref()
                                    && ui.button("Run").clicked()
                                {
                                    action.run_task_id = Some(task_id.clone());
                                }
                            });
                        }
                    });

                ui.add_space(14.0);
                show_sessions_section(ui, sessions, colors, &mut action);

                ui.add_space(14.0);
                show_activity_section(ui, activity, colors);
            });
        }
    }
    action
}

fn show_sessions_section(
    ui: &mut Ui,
    sessions: &[SessionInfo],
    colors: Palette,
    action: &mut AdminAction,
) {
    ui.label(
        RichText::new("DEVICES / ACTIVE SESSIONS")
            .size(11.0)
            .strong()
            .color(colors.muted),
    );
    if sessions.is_empty() {
        egui::Frame::group(ui.style())
            .fill(colors.panel)
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.label(RichText::new("No active sessions reported").color(colors.muted));
            });
        return;
    }

    for (index, session) in sessions.iter().enumerate() {
        let device_name =
            nonempty(session.device_name.as_deref()).unwrap_or_else(|| "Unknown device".to_owned());
        let client_label = match (
            nonempty(session.client.as_deref()),
            nonempty(session.application_version.as_deref()),
        ) {
            (Some(client), Some(version)) => Some(format!("{client} · {version}")),
            (Some(client), None) => Some(client),
            (None, Some(version)) => Some(version),
            (None, None) => None,
        };
        let user_name = nonempty(session.user_name.as_deref());
        let now_playing = session
            .now_playing_item
            .as_ref()
            .and_then(|item| nonempty(item.name.as_deref()))
            .map(|name| format!("Playing: {name}"));
        let last_activity = nonempty(session.last_activity_date.as_deref());

        egui::Frame::group(ui.style())
            .fill(colors.panel)
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(RichText::new(device_name).strong().color(colors.text));
                        let mut details = Vec::new();
                        if let Some(client) = client_label {
                            details.push(client);
                        }
                        if let Some(user) = user_name {
                            details.push(user);
                        }
                        if let Some(activity) = last_activity {
                            details.push(activity);
                        }
                        if !details.is_empty() {
                            ui.label(
                                RichText::new(details.join(" · "))
                                    .small()
                                    .color(colors.muted),
                            );
                        }
                        if let Some(playing) = now_playing {
                            ui.label(RichText::new(playing).small().color(colors.accent));
                        }
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let Some(device_id) = nonempty(session.device_id.as_deref())
                            && ui.button("Revoke").clicked()
                        {
                            action.revoke_device_id = Some(device_id);
                        }
                    });
                });

                if let Some(session_id) = nonempty(Some(session.id.as_str())) {
                    ui.add_space(6.0);
                    let draft_id = egui::Id::unique(("admin_session_msg", &session_id, index));
                    let mut draft = ui
                        .ctx()
                        .data(|data| data.get_temp::<String>(draft_id))
                        .unwrap_or_default();
                    let mut sent = false;
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut draft)
                                .desired_width(220.0)
                                .hint_text("Send message to device"),
                        );
                        let trimmed = draft.trim();
                        if ui
                            .add_enabled(!trimmed.is_empty(), egui::Button::new("Send"))
                            .clicked()
                        {
                            action.send_session_message =
                                Some((session_id.clone(), trimmed.to_owned()));
                            sent = true;
                        }
                    });
                    ui.ctx().data_mut(|data| {
                        if sent {
                            data.remove::<String>(draft_id);
                        } else {
                            data.insert_temp(draft_id, draft);
                        }
                    });
                }
            });
        ui.add_space(4.0);
    }
}

fn show_activity_section(ui: &mut Ui, activity: &[ActivityLogEntry], colors: Palette) {
    let filter_id = egui::Id::unique("admin_activity_filter");
    let mut filter = ui
        .ctx()
        .data(|data| data.get_temp::<ActivityFilter>(filter_id))
        .unwrap_or_default();

    ui.horizontal(|ui| {
        ui.label(
            RichText::new("ACTIVITY")
                .size(11.0)
                .strong()
                .color(colors.muted),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            for (tab, label) in [
                (ActivityFilter::System, "System"),
                (ActivityFilter::User, "User"),
                (ActivityFilter::All, "All"),
            ] {
                if ui.selectable_label(filter == tab, label).clicked() {
                    filter = tab;
                }
            }
        });
    });

    ui.ctx()
        .data_mut(|data| data.insert_temp(filter_id, filter));

    let filtered: Vec<&ActivityLogEntry> = activity
        .iter()
        .filter(|entry| {
            let has_user = nonempty(entry.user_id.as_deref()).is_some();
            match filter {
                ActivityFilter::All => true,
                ActivityFilter::User => has_user,
                ActivityFilter::System => !has_user,
            }
        })
        .collect();

    egui::Frame::group(ui.style())
        .fill(colors.panel)
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            if filtered.is_empty() {
                ui.label(RichText::new("No activity entries").color(colors.muted));
                return;
            }
            for entry in filtered {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        let name = nonempty(entry.name.as_deref())
                            .unwrap_or_else(|| "Activity".to_owned());
                        ui.label(RichText::new(name).strong().color(colors.text));
                        if let Some(overview) = nonempty(entry.short_overview.as_deref())
                            .or_else(|| nonempty(entry.overview.as_deref()))
                        {
                            ui.label(RichText::new(overview).small().color(colors.muted));
                        }
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let Some(date) = nonempty(entry.date.as_deref()) {
                            ui.label(RichText::new(date).small().color(colors.muted));
                        }
                        if let Some(severity) = nonempty(entry.severity.as_deref()) {
                            let severity_color = if severity.eq_ignore_ascii_case("Error")
                                || severity.eq_ignore_ascii_case("Fatal")
                            {
                                colors.danger
                            } else if severity.eq_ignore_ascii_case("Warn")
                                || severity.eq_ignore_ascii_case("Warning")
                            {
                                colors.warning
                            } else {
                                colors.muted
                            };
                            ui.label(RichText::new(severity).small().color(severity_color));
                        }
                    });
                });
                ui.add_space(4.0);
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(id: &str, name: &str) -> AdminUser {
        AdminUser {
            id: id.to_owned(),
            name: name.to_owned(),
            policy: Some(jellyfin_api::AdminUserPolicy::default()),
            last_activity_date: None,
        }
    }

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

    fn task(name: &str, state: &str, last: Option<&str>) -> ScheduledTask {
        ScheduledTask {
            id: None,
            name: Some(name.to_owned()),
            state: Some(state.to_owned()),
            current_progress_percentage: None,
            last_status: last.map(str::to_owned),
        }
    }

    /// A representative dashboard shape: three libraries, one active session,
    /// three users and two scheduled tasks.
    fn mock_snapshot() -> AdminSnapshot {
        let libraries = [
            library(Some("Movies")),
            library(Some("Shows")),
            library(Some("Music")),
        ];
        let users = UserCounts {
            total: 3,
            enabled: 2,
            disabled: 1,
        };
        let tasks = [
            task("Mock Library Scan", "Idle", Some("Completed")),
            task("Mock Cleanup", "Idle", None),
        ];
        AdminSnapshot::from_reads(
            &server_info(),
            &libraries,
            &[Some(74), None, Some(12)],
            1,
            users,
            &tasks,
            None,
            Vec::new(),
            Vec::new(),
        )
    }

    #[test]
    fn empty_library_list_shows_zero_libraries_and_no_name_rows() {
        let snapshot = AdminSnapshot::from_reads(
            &server_info(),
            &[],
            &[],
            0,
            UserCounts::default(),
            &[],
            None,
            Vec::new(),
            Vec::new(),
        );
        let view = dashboard_view(true, &AdminLoad::Loaded(Box::new(snapshot)));
        assert_eq!(
            view,
            DashboardView::Rows {
                rows: vec![
                    row("Server name", "Mock Jellyfin".to_owned()),
                    row("Product", "Jellyfin Server".to_owned()),
                    row("Operating system", "Linux".to_owned()),
                    row("Server version", "0.0.0-mock".to_owned()),
                    row("Libraries", "0".to_owned()),
                    row("Users", "0".to_owned()),
                    row("Active sessions", "0".to_owned()),
                    row("Scheduled tasks", "0".to_owned()),
                ],
                sessions: Vec::new(),
                activity: Vec::new(),
            }
        );
    }

    #[test]
    fn filled_data_lists_each_library_name_in_server_order() {
        let view = dashboard_view(true, &AdminLoad::Loaded(Box::new(mock_snapshot())));
        assert_eq!(
            view,
            DashboardView::Rows {
                rows: vec![
                    row("Server name", "Mock Jellyfin".to_owned()),
                    row("Product", "Jellyfin Server".to_owned()),
                    row("Operating system", "Linux".to_owned()),
                    row("Server version", "0.0.0-mock".to_owned()),
                    row("Libraries", "3".to_owned()),
                    row("Library", "Movies · 74 items".to_owned()),
                    row("Library", "Shows".to_owned()),
                    row("Library", "Music · 12 items".to_owned()),
                    row("Users", "3 · 1 disabled".to_owned()),
                    row("Active sessions", "1".to_owned()),
                    row("Scheduled tasks", "2".to_owned()),
                ],
                sessions: Vec::new(),
                activity: Vec::new(),
            }
        );
    }

    #[test]
    fn library_rows_show_an_item_count_only_when_the_server_reported_one() {
        let libraries = [library(Some("Movies")), library(Some("Shows"))];
        let snapshot = AdminSnapshot::from_reads(
            &server_info(),
            &libraries,
            &[Some(74), None],
            0,
            UserCounts::default(),
            &[],
            None,
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(snapshot.library_item_counts, vec![Some(74), None]);
        let DashboardView::Rows { rows, .. } =
            dashboard_view(true, &AdminLoad::Loaded(Box::new(snapshot)))
        else {
            panic!("expected rows");
        };
        assert_eq!(rows[5], row("Library", "Movies · 74 items".to_owned()));
        assert_eq!(rows[6], row("Library", "Shows".to_owned()));
    }

    #[test]
    fn users_row_reports_the_total_and_the_disabled_count() {
        let snapshot = AdminSnapshot::from_reads(
            &server_info(),
            &[],
            &[],
            0,
            UserCounts {
                total: 3,
                enabled: 3,
                disabled: 0,
            },
            &[],
            None,
            Vec::new(),
            Vec::new(),
        );
        let DashboardView::Rows { rows, .. } =
            dashboard_view(true, &AdminLoad::Loaded(Box::new(snapshot)))
        else {
            panic!("expected rows");
        };
        assert_eq!(rows[5], row("Users", "3".to_owned()));
    }

    #[test]
    fn only_tasks_that_run_or_last_failed_are_listed_individually() {
        let tasks = [
            task("Scan", "Idle", Some("Completed")),
            task("Cleanup", "Idle", None),
            task("Library scan", "Running", Some("Completed")),
            task("Metadata refresh", "Idle", Some("Failed")),
            task("Cancelled job", "Idle", Some("Aborted")),
        ];
        let snapshot = AdminSnapshot::from_reads(
            &server_info(),
            &[],
            &[],
            0,
            UserCounts::default(),
            &tasks,
            None,
            Vec::new(),
            Vec::new(),
        );
        let DashboardView::Rows { rows, .. } =
            dashboard_view(true, &AdminLoad::Loaded(Box::new(snapshot)))
        else {
            panic!("expected rows");
        };
        assert_eq!(
            rows[7],
            row(
                "Scheduled tasks",
                "5 · 1 running · 1 last failed".to_owned()
            )
        );
        assert_eq!(
            rows[8],
            row("Task", "Library scan · Running · Completed".to_owned())
        );
        assert_eq!(
            rows[9],
            row("Task", "Metadata refresh · Idle · Failed".to_owned())
        );
        assert_eq!(rows.len(), 10, "rows: {rows:?}");
    }

    #[test]
    fn task_detail_rows_are_capped_and_the_remainder_is_counted() {
        let tasks: Vec<ScheduledTask> = (0..MAX_TASK_DETAIL_ROWS + 2)
            .map(|index| task(&format!("Job {index}"), "Running", Some("Completed")))
            .collect();
        let snapshot = AdminSnapshot::from_reads(
            &server_info(),
            &[],
            &[],
            0,
            UserCounts::default(),
            &tasks,
            None,
            Vec::new(),
            Vec::new(),
        );
        let DashboardView::Rows { rows, .. } =
            dashboard_view(true, &AdminLoad::Loaded(Box::new(snapshot)))
        else {
            panic!("expected rows");
        };
        assert_eq!(
            rows[7],
            row(
                "Scheduled tasks",
                format!("{} · {} running", tasks.len(), tasks.len())
            )
        );
        assert_eq!(rows[8].label, ui_text(ids::ADMIN_ROW_TASK));
        assert_eq!(rows[8 + MAX_TASK_DETAIL_ROWS].value, "2 more not shown");
        assert_eq!(rows.len(), 8 + MAX_TASK_DETAIL_ROWS + 1);
    }

    #[test]
    fn task_lines_fall_back_for_missing_fields() {
        let tasks = [
            ScheduledTask {
                id: None,
                name: Some("  ".to_owned()),
                state: None,
                current_progress_percentage: None,
                last_status: Some("Failed".to_owned()),
            },
            ScheduledTask {
                id: None,
                name: Some("Never ran".to_owned()),
                state: Some("Idle".to_owned()),
                current_progress_percentage: None,
                last_status: None,
            },
        ];
        let snapshot = AdminSnapshot::from_reads(
            &server_info(),
            &[],
            &[],
            0,
            UserCounts::default(),
            &tasks,
            None,
            Vec::new(),
            Vec::new(),
        );
        let DashboardView::Rows { rows, .. } =
            dashboard_view(true, &AdminLoad::Loaded(Box::new(snapshot)))
        else {
            panic!("expected rows");
        };
        let not_reported = ui_text(ids::ADMIN_VALUE_NOT_REPORTED);
        assert_eq!(
            rows[8],
            row("Task", format!("{not_reported} · {not_reported} · Failed"))
        );
        assert_eq!(rows.len(), 9, "an idle task that never failed stays out");
    }

    #[test]
    fn unnamed_and_blank_libraries_are_counted_with_a_fallback_name() {
        let libraries = [
            library(None),
            library(Some("   ")),
            library(Some(" Photos ")),
        ];
        let snapshot = AdminSnapshot::from_reads(
            &server_info(),
            &libraries,
            &[],
            2,
            UserCounts::default(),
            &[],
            None,
            Vec::new(),
            Vec::new(),
        );
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
        let snapshot = AdminSnapshot::from_reads(
            &server_info(),
            &libraries,
            &[],
            0,
            UserCounts::default(),
            &[],
            None,
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(
            snapshot.library_names,
            vec!["Movies · movies · 2 folders", "Music · music"]
        );
    }

    #[test]
    fn missing_version_is_shown_as_not_reported() {
        let snapshot = AdminSnapshot::from_reads(
            &ServerInfo::default(),
            &[],
            &[],
            0,
            UserCounts::default(),
            &[],
            None,
            Vec::new(),
            Vec::new(),
        );
        let view = dashboard_view(true, &AdminLoad::Loaded(Box::new(snapshot)));
        let DashboardView::Rows { rows, .. } = view else {
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
            &AdminLoad::Loaded(Box::new(AdminSnapshot::from_reads(
                &ServerInfo::default(),
                &[],
                &[],
                0,
                UserCounts::default(),
                &[],
                None,
                Vec::new(),
                Vec::new(),
            ))),
        );
        let DashboardView::Rows { rows, .. } = view else {
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
            &AdminLoad::Loaded(Box::new(AdminSnapshot::from_reads(
                &server,
                &[],
                &[],
                0,
                UserCounts::default(),
                &[],
                None,
                Vec::new(),
                Vec::new(),
            ))),
        );
        let DashboardView::Rows { rows, .. } = view else {
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
        let view = dashboard_view(false, &AdminLoad::Loaded(Box::new(mock_snapshot())));
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

    #[test]
    fn user_list_requires_an_explicit_load_and_ignores_stale_responses() {
        let mut state = AdminUsersData::default();
        assert!(!state.begin_list(10, 7));
        state.reset(7);
        assert!(state.begin_list(10, 7));
        assert!(
            !state.begin_list(11, 7),
            "only one operation may be pending"
        );
        assert!(!state.finish_list(9, 7, Ok(vec![account("u1", "Old")])));
        assert!(state.is_loading());
        assert!(!state.finish_list(10, 6, Ok(vec![account("u1", "Old")])));
        assert!(state.finish_list(10, 7, Ok(vec![account("u1", "Riley")])));
        assert_eq!(state.users[0].name, "Riley");
        assert!(!state.is_loading());
    }

    #[test]
    fn user_mutation_completion_is_correlated_and_updates_only_the_local_snapshot() {
        let mut state = AdminUsersData::default();
        state.reset(3);
        state.users.push(account("u1", "Before"));
        assert!(state.begin_mutation(
            22,
            3,
            PendingAdminUserMutation::Rename {
                user_id: "u1".to_owned(),
                name: "After".to_owned(),
            },
        ));
        assert!(!state.finish_mutation(21, 3, AdminUserMutationKind::Rename, Ok(None),));
        assert!(!state.finish_mutation(22, 3, AdminUserMutationKind::Delete, Ok(None),));
        assert!(state.is_loading());
        assert!(state.finish_mutation(22, 3, AdminUserMutationKind::Rename, Ok(None),));
        assert_eq!(state.users[0].name, "After");
        assert!(!state.is_loading());
    }

    #[test]
    fn changing_connection_generation_discards_pending_user_data_and_errors() {
        let mut state = AdminUsersData::default();
        state.reset(4);
        assert!(state.begin_list(30, 4));
        assert!(state.finish_list(30, 4, Err(ErrorClass::Forbidden)));
        assert!(state.error.is_some());
        state.users.push(account("u1", "Riley"));

        state.reset(5);

        assert_eq!(state.generation, 5);
        assert!(state.users.is_empty());
        assert!(state.error.is_none());
        assert!(!state.is_loading());
        assert!(!state.finish_list(30, 4, Ok(vec![account("u1", "Stale")])));
    }

    fn collect_texts(shapes: &[egui::epaint::ClippedShape]) -> Vec<(f32, String)> {
        fn visit(shape: &egui::epaint::Shape, out: &mut Vec<(f32, String)>) {
            match shape {
                egui::epaint::Shape::Text(text) => {
                    out.push((text.pos.y, text.galley.job.text.clone()));
                }
                egui::epaint::Shape::Vec(nested) => {
                    for child in nested {
                        visit(child, out);
                    }
                }
                _ => {}
            }
        }
        let mut out = Vec::new();
        for clipped in shapes {
            visit(&clipped.shape, &mut out);
        }
        out
    }

    fn find_text_y(texts: &[(f32, String)], needle: &str) -> Option<f32> {
        texts
            .iter()
            .find(|(_, text)| text.contains(needle))
            .map(|(y, _)| *y)
    }

    #[test]
    fn dashboard_renders_wide_and_narrow_without_subtitle() {
        let view = dashboard_view(true, &AdminLoad::Loaded(Box::new(mock_snapshot())));
        let colors = crate::theme::palette();
        for width in [600.0_f32, 960.0] {
            let context = egui::Context::default();
            let mut output = context.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(width, 1200.0),
                    )),
                    ..Default::default()
                },
                |ui| {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        let _ = show(ui, &view, colors, false);
                    });
                },
            );
            output.textures_delta.clear();
            let texts = collect_texts(&output.shapes);
            assert!(
                find_text_y(&texts, "Read-only overview of this Jellyfin server.").is_none(),
                "subtitle should be removed at {width}: {texts:?}"
            );
            let server_y = find_text_y(&texts, "SERVER")
                .unwrap_or_else(|| panic!("SERVER heading missing at {width}: {texts:?}"));
            let libraries_y = find_text_y(&texts, "LIBRARIES")
                .unwrap_or_else(|| panic!("LIBRARIES heading missing at {width}: {texts:?}"));
            let tasks_y = find_text_y(&texts, "SCHEDULED TASKS")
                .unwrap_or_else(|| panic!("SCHEDULED TASKS heading missing at {width}: {texts:?}"));
            if width >= 720.0 {
                assert!(
                    (server_y - libraries_y).abs() < 2.0,
                    "SERVER ({server_y}) and LIBRARIES ({libraries_y}) should align side-by-side at {width}"
                );
            } else {
                assert!(
                    libraries_y > server_y,
                    "LIBRARIES ({libraries_y}) should stack below SERVER ({server_y}) at {width}"
                );
            }
            assert!(
                tasks_y > libraries_y,
                "SCHEDULED TASKS ({tasks_y}) should render below LIBRARIES ({libraries_y})"
            );
        }
    }

    #[test]
    fn manage_users_button_aligns_with_dashboard_heading_inside_card() {
        let view = dashboard_view(true, &AdminLoad::Loaded(Box::new(mock_snapshot())));
        let colors = crate::theme::palette();
        let context = egui::Context::default();
        let mut output = context.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(960.0, 900.0),
                )),
                ..Default::default()
            },
            |ui| {
                let _ = show(ui, &view, colors, true);
            },
        );
        output.textures_delta.clear();
        let texts = collect_texts(&output.shapes);
        let heading_y = find_text_y(&texts, ui_text(ids::ADMIN_HEADING_TITLE))
            .expect("dashboard heading missing");
        let manage_y = find_text_y(&texts, "Manage users").expect("Manage users button missing");
        assert!(
            (heading_y - manage_y).abs() < 10.0,
            "Manage users ({manage_y}) should align on the Server dashboard heading row ({heading_y})"
        );
    }

    #[test]
    fn item_counts_render_four_cards_and_running_task_shows_progress() {
        let snapshot = AdminSnapshot::from_reads(
            &server_info(),
            &[],
            &[],
            0,
            UserCounts::default(),
            &[ScheduledTask {
                id: Some("task-scan".to_owned()),
                name: Some("Scan".to_owned()),
                state: Some("Running".to_owned()),
                current_progress_percentage: Some(42.0),
                last_status: Some("Completed".to_owned()),
            }],
            Some(jellyfin_api::ItemCounts {
                movie_count: Some(10),
                series_count: Some(5),
                episode_count: Some(120),
                box_set_count: Some(3),
            }),
            Vec::new(),
            Vec::new(),
        );
        let view = dashboard_view(true, &AdminLoad::Loaded(Box::new(snapshot)));
        let colors = crate::theme::palette();
        let context = egui::Context::default();
        let mut output = context.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(960.0, 900.0),
                )),
                ..Default::default()
            },
            |ui| {
                let _ = show(ui, &view, colors, true);
            },
        );
        output.textures_delta.clear();
        let texts = collect_texts(&output.shapes);
        assert!(find_text_y(&texts, "Movies").is_some());
        assert!(find_text_y(&texts, "Series").is_some());
        assert!(find_text_y(&texts, "Episodes").is_some());
        assert!(find_text_y(&texts, "Collections").is_some());
        assert!(find_text_y(&texts, "Scan · Running (42%) · Completed").is_some());
        assert!(find_text_y(&texts, "Run").is_some());
        assert!(find_text_y(&texts, "Refresh").is_some());
        assert!(find_text_y(&texts, "Scan All Libraries").is_some());
    }

    #[test]
    fn sessions_and_activity_sections_render_and_revoke_emits_device_id() {
        let snapshot = AdminSnapshot::from_reads(
            &server_info(),
            &[],
            &[],
            1,
            UserCounts::default(),
            &[],
            None,
            vec![SessionInfo {
                id: "sess-1".to_owned(),
                device_id: Some("dev-1".to_owned()),
                device_name: Some("Living Room TV".to_owned()),
                client: Some("Jellyfin Kattri".to_owned()),
                application_version: Some("0.7.0".to_owned()),
                user_name: Some("Ada".to_owned()),
                last_activity_date: Some("2026-11-27T12:00:00Z".to_owned()),
                now_playing_item: Some(jellyfin_api::SessionNowPlayingItem {
                    name: Some("The Matrix".to_owned()),
                }),
            }],
            vec![
                ActivityLogEntry {
                    id: Some(1),
                    name: Some("User signed in".to_owned()),
                    short_overview: Some("Ada signed in".to_owned()),
                    overview: None,
                    entry_type: None,
                    date: Some("2026-11-27T12:00:00Z".to_owned()),
                    severity: Some("Information".to_owned()),
                    user_id: Some("u1".to_owned()),
                },
                ActivityLogEntry {
                    id: Some(2),
                    name: Some("Plugin updated".to_owned()),
                    short_overview: None,
                    overview: Some("Updated metadata plugin".to_owned()),
                    entry_type: None,
                    date: Some("2026-11-27T11:00:00Z".to_owned()),
                    severity: Some("Warn".to_owned()),
                    user_id: None,
                },
            ],
        );
        let view = dashboard_view(true, &AdminLoad::Loaded(Box::new(snapshot)));
        let colors = crate::theme::palette();
        let context = egui::Context::default();
        let mut output = context.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(960.0, 1400.0),
                )),
                ..Default::default()
            },
            |ui| {
                let _ = show(ui, &view, colors, true);
            },
        );
        output.textures_delta.clear();
        let texts = collect_texts(&output.shapes);
        assert!(find_text_y(&texts, "DEVICES / ACTIVE SESSIONS").is_some());
        assert!(find_text_y(&texts, "Living Room TV").is_some());
        assert!(find_text_y(&texts, "Playing: The Matrix").is_some());
        assert!(find_text_y(&texts, "Revoke").is_some());
        assert!(find_text_y(&texts, "ACTIVITY").is_some());
        assert!(find_text_y(&texts, "User signed in").is_some());
        assert!(find_text_y(&texts, "Plugin updated").is_some());
    }

    #[test]
    fn server_power_buttons_require_two_clicks_to_confirm() {
        let view = dashboard_view(true, &AdminLoad::Loaded(Box::new(mock_snapshot())));
        let colors = crate::theme::palette();
        let context = egui::Context::default();

        let mut output = context.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(960.0, 1200.0),
                )),
                ..Default::default()
            },
            |ui| {
                let action = show(ui, &view, colors, false);
                assert!(!action.restart_server);
                assert!(!action.shutdown_server);
            },
        );
        output.textures_delta.clear();
        let texts = collect_texts(&output.shapes);
        let restart_y = find_text_y(&texts, "Restart").expect("Restart button missing");
        assert!(find_text_y(&texts, "Confirm Restart").is_none());
        let _ = restart_y;
    }
}

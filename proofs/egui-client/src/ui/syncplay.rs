//! Native SyncPlay group discovery and membership controls.

use egui::{self, Color32, RichText};
use jellyfin_api::{GroupState, SyncPlayGroup};

use crate::app::KattriApp;
use crate::theme::Palette;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum LoadState {
    #[default]
    NotLoaded,
    Loading,
    Loaded,
    Failed,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum CreateState {
    #[default]
    Idle,
    Creating,
    Failed,
}

/// UI state for the Watch Together screen. Group IDs stay in this internal model and are never
/// rendered, copied to accessibility labels, or included in user-facing errors.
#[derive(Default)]
pub(crate) struct SyncPlayScreenState {
    groups: Vec<SyncPlayGroup>,
    load_state: LoadState,
    load_request: Option<u64>,
    create_state: CreateState,
    create_request: Option<u64>,
    pub(crate) group_name: String,
    action_error: bool,
}

impl SyncPlayScreenState {
    pub(crate) fn groups(&self) -> &[SyncPlayGroup] {
        &self.groups
    }

    pub(crate) fn upsert_group(&mut self, group: SyncPlayGroup) {
        if let Some(existing) = self
            .groups
            .iter_mut()
            .find(|existing| existing.group_id == group.group_id)
        {
            *existing = group;
        } else {
            self.groups.push(group);
        }
        self.load_state = LoadState::Loaded;
    }

    pub(crate) fn update_participant(
        &mut self,
        group_id: Option<&str>,
        user_name: &str,
        joined: bool,
    ) {
        let Some(group_id) = group_id else {
            return;
        };
        let Some(group) = self
            .groups
            .iter_mut()
            .find(|group| group.group_id == group_id)
        else {
            return;
        };
        if joined {
            if !group.participants.iter().any(|name| name == user_name) {
                group.participants.push(user_name.to_owned());
            }
        } else {
            group.participants.retain(|name| name != user_name);
        }
    }

    pub(crate) fn update_group_state(&mut self, group_id: Option<&str>, state: GroupState) {
        let Some(group_id) = group_id else {
            return;
        };
        if let Some(group) = self
            .groups
            .iter_mut()
            .find(|group| group.group_id == group_id)
        {
            group.state = state;
        }
    }

    pub(crate) fn is_loading(&self) -> bool {
        self.load_state == LoadState::Loading
    }

    pub(crate) fn list_is_unloaded(&self) -> bool {
        self.load_state == LoadState::NotLoaded
    }

    pub(crate) fn has_list_failed(&self) -> bool {
        self.load_state == LoadState::Failed
    }

    pub(crate) fn has_loaded_groups(&self) -> bool {
        self.load_state == LoadState::Loaded
    }

    pub(crate) fn create_has_failed(&self) -> bool {
        self.create_state == CreateState::Failed
    }

    pub(crate) fn has_action_error(&self) -> bool {
        self.action_error
    }

    pub(crate) fn begin_list(&mut self, request_id: u64) {
        self.load_request = Some(request_id);
        self.load_state = LoadState::Loading;
        self.action_error = false;
    }

    pub(crate) fn list_loaded(&mut self, request_id: u64, groups: Vec<SyncPlayGroup>) -> bool {
        if self.load_request != Some(request_id) {
            return false;
        }
        self.load_request = None;
        self.load_state = LoadState::Loaded;
        self.groups = groups;
        true
    }

    pub(crate) fn list_failed(&mut self, request_id: u64) -> bool {
        if self.load_request != Some(request_id) {
            return false;
        }
        self.load_request = None;
        self.load_state = LoadState::Failed;
        true
    }

    pub(crate) fn begin_create(&mut self, request_id: u64) {
        self.create_request = Some(request_id);
        self.create_state = CreateState::Creating;
        self.action_error = false;
    }

    pub(crate) fn group_created(&mut self, request_id: u64, group: SyncPlayGroup) -> bool {
        if self.create_request != Some(request_id) {
            return false;
        }
        self.create_request = None;
        self.create_state = CreateState::Idle;
        self.group_name.clear();
        if let Some(existing) = self
            .groups
            .iter_mut()
            .find(|item| item.group_id == group.group_id)
        {
            *existing = group;
        } else {
            self.groups.push(group);
        }
        self.load_state = LoadState::Loaded;
        self.action_error = false;
        true
    }

    pub(crate) fn create_failed(&mut self, request_id: u64) -> bool {
        if self.create_request != Some(request_id) {
            return false;
        }
        self.create_request = None;
        self.create_state = CreateState::Failed;
        self.action_error = true;
        true
    }

    pub(crate) fn action_failed(&mut self) {
        self.action_error = true;
    }

    pub(crate) fn action_succeeded(&mut self) {
        self.action_error = false;
    }

    pub(crate) fn creating(&self) -> bool {
        self.create_state == CreateState::Creating
    }
}

pub(crate) fn show(ui: &mut egui::Ui, app: &mut KattriApp, colors: Palette) {
    ui.heading(RichText::new("Watch Together").color(colors.text));
    ui.add_space(6.0);
    ui.label(RichText::new("Create a group or join a room to watch in sync.").color(colors.muted));
    ui.add_space(20.0);

    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("New group").strong().color(colors.text));
        ui.add_space(8.0);
        let input = ui.add_sized(
            [(ui.available_width() - 170.0).clamp(100.0, 320.0), 38.0],
            egui::TextEdit::singleline(&mut app.sync_play_ui.group_name)
                .hint_text("Group name")
                .id_salt("syncplay_group_name"),
        );
        input.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Group name"));
        let create = ui.add_enabled(
            !app.sync_play_ui.creating()
                && app.joined_sync_play_group_id().is_none()
                && !app.sync_play_ui.group_name.trim().is_empty(),
            egui::Button::new(if app.sync_play_ui.creating() {
                "Creating…"
            } else {
                "Create group"
            }),
        );
        accessible_button(&create, "Create SyncPlay group");
        if create.clicked() {
            app.create_sync_play_group_from_ui();
        }
    });

    ui.add_space(20.0);
    ui.horizontal(|ui| {
        ui.heading(RichText::new("Available groups").color(colors.text));
        ui.add_space(10.0);
        let refresh = ui.button("Refresh");
        accessible_button(&refresh, "Refresh available SyncPlay groups");
        if refresh.clicked() {
            app.request_sync_play_groups();
        }
        if app.sync_play_ui.is_loading() {
            ui.spinner();
            ui.label(RichText::new("Loading groups…").color(colors.muted));
        }
    });

    if app.sync_play_ui.has_action_error()
        || app.sync_play_ui.has_list_failed()
        || app.sync_play_ui.create_has_failed()
    {
        ui.colored_label(
            Color32::from_rgb(245, 130, 130),
            "Jellyfin could not complete that group action. Check your SyncPlay permission and connection.",
        );
    }

    if app.joined_sync_play_group_id().is_none() {
        ui.label(RichText::new("You are not in a group.").color(colors.muted));
    } else {
        let group_name = app
            .joined_sync_play_group_id()
            .and_then(|id| {
                app.sync_play_ui
                    .groups()
                    .iter()
                    .find(|group| group.group_id == id)
            })
            .map(|group| group.group_name.as_str())
            .filter(|name| !name.is_empty())
            .unwrap_or("a group");
        ui.label(RichText::new(format!("Joined: {group_name}")).color(colors.accent));
        let leave = ui.add_enabled(
            !app.sync_play_leave_pending(),
            egui::Button::new(if app.sync_play_leave_pending() {
                "Leaving…"
            } else {
                "Leave group"
            }),
        );
        accessible_button(&leave, "Leave SyncPlay group");
        if leave.clicked() && app.leave_sync_play_group().is_err() {
            app.sync_play_ui.action_failed();
        }
    }

    ui.add_space(8.0);
    match (
        app.sync_play_ui.list_is_unloaded(),
        app.sync_play_ui.has_list_failed(),
        app.sync_play_ui.has_loaded_groups(),
    ) {
        (true, _, _) if app.sync_play_ui.groups().is_empty() => {
            ui.label(RichText::new("Choose Refresh to find active groups.").color(colors.muted));
        }
        (_, true, _) if app.sync_play_ui.groups().is_empty() => {
            let retry = ui.button("Try again");
            accessible_button(&retry, "Retry loading SyncPlay groups");
            if retry.clicked() {
                app.request_sync_play_groups();
            }
        }
        (_, _, true) if app.sync_play_ui.groups().is_empty() => {
            ui.label(
                RichText::new("No active groups yet. Create one to get started.")
                    .color(colors.muted),
            );
        }
        _ => {
            let joined_id = app.joined_sync_play_group_id().map(str::to_owned);
            let groups = app.sync_play_ui.groups().to_vec();
            egui::ScrollArea::vertical()
                .id_salt("syncplay_groups")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for group in &groups {
                        let display_name = if group.group_name.trim().is_empty() {
                            "Unnamed group"
                        } else {
                            group.group_name.as_str()
                        };
                        egui::Frame::group(ui.style())
                            .fill(colors.panel)
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.vertical(|ui| {
                                        ui.label(
                                            RichText::new(display_name).strong().color(colors.text),
                                        );
                                        ui.label(
                                            RichText::new(format!(
                                                "{} · {:?}",
                                                participant_count(group.participants.len()),
                                                group.state
                                            ))
                                            .color(colors.muted),
                                        );
                                        if !group.participants.is_empty() {
                                            ui.label(
                                                RichText::new(participant_summary(
                                                    &group.participants,
                                                ))
                                                .size(12.0)
                                                .color(colors.muted),
                                            );
                                        }
                                    });
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            let already_joined = joined_id.as_deref()
                                                == Some(group.group_id.as_str());
                                            let join = ui.add_enabled(
                                                !already_joined
                                                    && joined_id.is_none()
                                                    && !app.sync_play_join_pending(),
                                                egui::Button::new(if already_joined {
                                                    "Joined"
                                                } else {
                                                    "Join"
                                                }),
                                            );
                                            let access_name = if already_joined {
                                                format!(
                                                    "Already joined SyncPlay group {display_name}"
                                                )
                                            } else {
                                                format!("Join SyncPlay group {display_name}")
                                            };
                                            accessible_button(&join, &access_name);
                                            if join.clicked()
                                                && app
                                                    .join_sync_play_group(group.group_id.clone())
                                                    .is_err()
                                            {
                                                app.sync_play_ui.action_failed();
                                            }
                                            if app.sync_play_join_pending() && !already_joined {
                                                ui.spinner();
                                                ui.label(
                                                    RichText::new("Joining…").color(colors.muted),
                                                );
                                            }
                                        },
                                    );
                                });
                            });
                        ui.add_space(8.0);
                    }
                });
        }
    }
}

fn participant_count(count: usize) -> String {
    if count == 1 {
        "1 participant".to_owned()
    } else {
        format!("{count} participants")
    }
}

fn participant_summary(participants: &[String]) -> String {
    let names = participants
        .iter()
        .take(3)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    if participants.len() > 3 {
        format!("{names} +{} more", participants.len() - 3)
    } else {
        names
    }
}

fn accessible_button(response: &egui::Response, name: &str) {
    let enabled = response.enabled();
    let name = name.to_owned();
    response
        .widget_info(move || egui::WidgetInfo::labeled(egui::Role::Button, enabled, name.clone()));
}

#[cfg(test)]
mod tests {
    use super::{CreateState, LoadState, SyncPlayScreenState};
    use jellyfin_api::{GroupState, SyncPlayGroup};

    fn group(id: &str, name: &str) -> SyncPlayGroup {
        SyncPlayGroup {
            group_id: id.to_owned(),
            group_name: name.to_owned(),
            state: GroupState::Idle,
            participants: Vec::new(),
            last_updated_at: String::new(),
        }
    }

    #[test]
    fn list_loading_success_empty_and_failure_states_are_request_scoped() {
        let mut state = SyncPlayScreenState::default();
        state.begin_list(4);
        assert_eq!(state.load_state, LoadState::Loading);
        assert!(!state.list_failed(3));
        assert_eq!(state.load_state, LoadState::Loading);
        assert!(state.list_loaded(4, Vec::new()));
        assert_eq!(state.load_state, LoadState::Loaded);
        state.begin_list(5);
        assert!(state.list_failed(5));
        assert_eq!(state.load_state, LoadState::Failed);
    }

    #[test]
    fn creating_updates_the_local_list_without_showing_identifiers() {
        let mut state = SyncPlayScreenState {
            group_name: "Movie night".to_owned(),
            ..SyncPlayScreenState::default()
        };
        state.begin_create(8);
        assert_eq!(state.create_state, CreateState::Creating);
        assert!(!state.group_created(7, group("private-id", "Movie night")));
        assert!(state.group_created(8, group("private-id", "Movie night")));
        assert!(state.group_name.is_empty());
        assert_eq!(state.groups.len(), 1);
        assert_eq!(state.groups[0].group_name, "Movie night");
    }

    #[test]
    fn stale_create_failure_does_not_replace_current_state() {
        let mut state = SyncPlayScreenState::default();
        state.begin_create(10);
        assert!(!state.create_failed(9));
        assert_eq!(state.create_state, CreateState::Creating);
        assert!(state.create_failed(10));
        assert_eq!(state.create_state, CreateState::Failed);
    }

    #[test]
    fn group_updates_keep_the_local_list_and_participant_count_current() {
        let mut state = SyncPlayScreenState::default();
        state.upsert_group(group("group-1", "Movie night"));
        state.update_participant(Some("group-1"), "Alex", true);
        state.update_participant(Some("group-1"), "Alex", true);
        state.update_participant(Some("group-1"), "Sam", true);
        state.update_group_state(Some("group-1"), GroupState::Playing);

        assert_eq!(state.groups()[0].participants, ["Alex", "Sam"]);
        assert_eq!(state.groups()[0].state, GroupState::Playing);

        state.update_participant(Some("group-1"), "Alex", false);
        state.update_participant(Some("unknown-group"), "Sam", false);
        assert_eq!(state.groups()[0].participants, ["Sam"]);
    }
}

//! Native user-management view. This component owns only temporary form state
//! and emits typed intents; the caller performs authorization and API work.

use std::fmt;

use egui::{Align, Layout, RichText};
use jellyfin_api::{AdminUser, AdminUserPolicy, AdminUserPolicyChanges};
use zeroize::{Zeroize, Zeroizing};

const MAX_USER_NAME_BYTES: usize = 256;
const MAX_PASSWORD_BYTES: usize = 4_096;

/// Immutable server snapshot supplied by the parent view.
#[derive(Clone, Copy, Debug)]
pub struct AdminUsersSnapshot<'a> {
    /// Users returned by the parent worker, in the order it chose to display them.
    pub users: &'a [AdminUser],
    /// True while the parent is fetching or mutating users.
    pub loading: bool,
    /// Sanitized caller-provided error text, if a previous request failed.
    pub error: Option<&'a str>,
    /// Signed-in account ID. If unavailable, deletion stays disabled.
    pub current_user_id: Option<&'a str>,
}

/// Actions the parent handles through its worker. Returning an action does not
/// claim the server accepted it.
pub enum AdminUsersAction {
    /// Fetch the current user list.
    Refresh,
    /// Create an account. The password leaves the component only on submission.
    Create {
        name: String,
        password: Zeroizing<String>,
    },
    /// Change one user's display name.
    Rename { user_id: String, name: String },
    /// Patch selected policy flags for one user.
    PolicyPatch {
        user_id: String,
        changes: AdminUserPolicyChanges,
    },
    /// Delete the explicitly confirmed user.
    Delete { user_id: String },
}

impl fmt::Debug for AdminUsersAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refresh => formatter.write_str("Refresh"),
            Self::Create { name, .. } => formatter
                .debug_struct("Create")
                .field("name", name)
                .field("password", &"[REDACTED]")
                .finish(),
            Self::Rename { user_id, name } => formatter
                .debug_struct("Rename")
                .field("user_id", user_id)
                .field("name", name)
                .finish(),
            Self::PolicyPatch { user_id, changes } => formatter
                .debug_struct("PolicyPatch")
                .field("user_id", user_id)
                .field("changes", changes)
                .finish(),
            Self::Delete { user_id } => formatter
                .debug_struct("Delete")
                .field("user_id", user_id)
                .finish(),
        }
    }
}

/// Persistent component state. Call [`AdminUsersState::cancel`] when the parent
/// closes or leaves this view; it clears drafts and zeroizes any unsent password.
pub struct AdminUsersState {
    create_open: bool,
    create_name: String,
    create_password: Zeroizing<String>,
    create_error: Option<&'static str>,
    rename: Option<RenameDraft>,
    policy: Option<PolicyDraft>,
    delete: Option<DeleteDraft>,
    pending_policy: Option<PendingPolicyPatch>,
}

impl Default for AdminUsersState {
    fn default() -> Self {
        Self {
            create_open: false,
            create_name: String::new(),
            create_password: Zeroizing::new(String::new()),
            create_error: None,
            rename: None,
            policy: None,
            delete: None,
            pending_policy: None,
        }
    }
}

impl fmt::Debug for AdminUsersState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AdminUsersState")
            .field("create_open", &self.create_open)
            .field("create_name", &self.create_name)
            .field("create_password", &"[REDACTED]")
            .field("create_error", &self.create_error)
            .field("rename", &self.rename)
            .field("policy", &self.policy)
            .field("delete", &self.delete)
            .field("pending_policy", &self.pending_policy)
            .finish()
    }
}

impl AdminUsersState {
    /// Cancel all open forms and confirmations, clearing the password draft.
    /// The parent should call this when the user navigates away from the view.
    pub fn cancel(&mut self) {
        self.cancel_create();
        self.rename = None;
        self.policy = None;
        self.delete = None;
        self.pending_policy = None;
    }

    fn open_create(&mut self) {
        self.cancel();
        self.create_open = true;
    }

    fn open_rename(&mut self, user: &AdminUser) {
        self.cancel();
        self.rename = Some(RenameDraft {
            user_id: user.id.clone(),
            current_name: user.name.clone(),
            name: user.name.clone(),
            error: None,
        });
    }

    fn open_policy(&mut self, user: &AdminUser) {
        let Some(policy) = user.policy.as_ref() else {
            return;
        };
        self.cancel();
        let flags = PolicyFlags::from(policy);
        self.policy = Some(PolicyDraft {
            user_id: user.id.clone(),
            user_name: user.name.clone(),
            original: flags,
            edited: flags,
        });
    }

    fn open_delete(&mut self, user: &AdminUser, snapshot: AdminUsersSnapshot<'_>) {
        let Some(current_user_id) = snapshot.current_user_id else {
            return;
        };
        if !current_user_is_known(snapshot) || same_user_id(current_user_id, &user.id) {
            return;
        }
        self.cancel();
        self.delete = Some(DeleteDraft {
            user_id: user.id.clone(),
            target_name: user.name.clone(),
            typed_name: String::new(),
        });
    }

    fn cancel_create(&mut self) {
        self.create_password.zeroize();
        self.create_password.clear();
        self.create_name.clear();
        self.create_error = None;
        self.create_open = false;
    }

    fn submit_create(&mut self) -> Option<AdminUsersAction> {
        let name = match validate_user_name(&self.create_name) {
            Ok(name) => name,
            Err(NameError::Blank) => {
                self.create_error = Some("Enter a user name.");
                return None;
            }
            Err(NameError::TooLong) => {
                self.create_error = Some("User names must be 256 bytes or fewer.");
                return None;
            }
            Err(NameError::ControlCharacter) => {
                self.create_error = Some("User names cannot contain control characters.");
                return None;
            }
        };
        if self.create_password.is_empty() {
            self.create_error = Some("Enter an initial password.");
            return None;
        }
        if self.create_password.len() > MAX_PASSWORD_BYTES {
            self.create_error = Some("Passwords must be 4,096 bytes or fewer.");
            return None;
        }

        let password = std::mem::replace(&mut self.create_password, Zeroizing::new(String::new()));
        self.create_open = false;
        self.create_name.clear();
        self.create_error = None;
        Some(AdminUsersAction::Create { name, password })
    }

    fn stage_policy_patch(&mut self, actions: &mut Vec<AdminUsersAction>) {
        let Some(draft) = self.policy.as_ref() else {
            return;
        };
        let pending = PendingPolicyPatch {
            user_id: draft.user_id.clone(),
            user_name: draft.user_name.clone(),
            changes: draft.changes(),
        };
        if policy_changes_empty(&pending.changes) {
            self.policy = None;
        } else if needs_privilege_confirmation(&pending.changes) {
            self.pending_policy = Some(pending);
        } else {
            self.policy = None;
            actions.push(AdminUsersAction::PolicyPatch {
                user_id: pending.user_id,
                changes: pending.changes,
            });
        }
    }

    fn resolve_policy_confirmation(
        &mut self,
        confirmed: bool,
        actions: &mut Vec<AdminUsersAction>,
    ) {
        let Some(pending) = self.pending_policy.take() else {
            return;
        };
        if confirmed {
            self.policy = None;
            actions.push(AdminUsersAction::PolicyPatch {
                user_id: pending.user_id,
                changes: pending.changes,
            });
        }
    }

    fn delete_confirmation_ready(&self, snapshot: AdminUsersSnapshot<'_>) -> bool {
        let Some(draft) = self.delete.as_ref() else {
            return false;
        };
        delete_confirmation_ready(snapshot, draft)
    }

    fn confirm_delete(&mut self, snapshot: AdminUsersSnapshot<'_>) -> Option<AdminUsersAction> {
        if !self.delete_confirmation_ready(snapshot) {
            return None;
        }
        let draft = self.delete.take()?;
        Some(AdminUsersAction::Delete {
            user_id: draft.user_id,
        })
    }
}

#[derive(Debug)]
struct RenameDraft {
    user_id: String,
    current_name: String,
    name: String,
    error: Option<&'static str>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PolicyFlags {
    is_administrator: Option<bool>,
    is_hidden: Option<bool>,
    is_disabled: Option<bool>,
    enable_media_playback: Option<bool>,
    enable_remote_access: Option<bool>,
}

impl From<&AdminUserPolicy> for PolicyFlags {
    fn from(policy: &AdminUserPolicy) -> Self {
        Self {
            is_administrator: policy.is_administrator,
            is_hidden: policy.is_hidden,
            is_disabled: policy.is_disabled,
            enable_media_playback: policy.enable_media_playback,
            enable_remote_access: policy.enable_remote_access,
        }
    }
}

#[derive(Debug)]
struct PolicyDraft {
    user_id: String,
    user_name: String,
    original: PolicyFlags,
    edited: PolicyFlags,
}

impl PolicyDraft {
    fn changes(&self) -> AdminUserPolicyChanges {
        AdminUserPolicyChanges {
            is_administrator: changed_value(
                self.original.is_administrator,
                self.edited.is_administrator,
            ),
            is_hidden: changed_value(self.original.is_hidden, self.edited.is_hidden),
            is_disabled: changed_value(self.original.is_disabled, self.edited.is_disabled),
            enable_media_playback: changed_value(
                self.original.enable_media_playback,
                self.edited.enable_media_playback,
            ),
            enable_remote_access: changed_value(
                self.original.enable_remote_access,
                self.edited.enable_remote_access,
            ),
        }
    }
}

#[derive(Debug)]
struct DeleteDraft {
    user_id: String,
    target_name: String,
    typed_name: String,
}

#[derive(Debug)]
struct PendingPolicyPatch {
    user_id: String,
    user_name: String,
    changes: AdminUserPolicyChanges,
}

/// Draw the management screen and return user intents for the parent worker.
/// No request is emitted merely by opening or rendering the view.
pub fn show(
    ui: &mut egui::Ui,
    snapshot: AdminUsersSnapshot<'_>,
    state: &mut AdminUsersState,
) -> Vec<AdminUsersAction> {
    let colors = crate::theme::active_palette();
    let mut actions = Vec::new();

    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.heading(
                RichText::new("Users")
                    .size(28.0)
                    .strong()
                    .color(colors.text),
            );
            ui.label(RichText::new("Manage accounts and server access.").color(colors.muted));
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui
                .add_enabled(!snapshot.loading, egui::Button::new("Add user"))
                .clicked()
            {
                state.open_create();
            }
            if ui
                .add_enabled(
                    !snapshot.loading,
                    egui::Button::new(if snapshot.loading {
                        "Refreshing…"
                    } else {
                        "Refresh"
                    }),
                )
                .clicked()
            {
                actions.push(AdminUsersAction::Refresh);
            }
        });
    });

    ui.add_space(14.0);
    if snapshot.loading {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(RichText::new("Loading users").color(colors.muted));
        });
    }
    if let Some(error) = snapshot.error {
        ui.colored_label(colors.danger, error);
    }

    ui.add_space(8.0);
    if snapshot.users.is_empty() {
        if !snapshot.loading {
            ui.label(RichText::new("No user accounts found.").color(colors.muted));
        }
    } else {
        egui::ScrollArea::vertical()
            .id_salt("admin-users-list")
            .auto_shrink([false, true])
            .max_height(ui.available_height().max(180.0))
            .show(ui, |ui| {
                ui.vertical(|ui| {
                    for user in snapshot.users {
                        user_row(ui, user, snapshot, state, colors);
                        ui.add_space(8.0);
                    }
                });
            });
    }

    draw_active_dialog(ui.ctx(), snapshot, state, &mut actions, colors);
    actions
}

fn user_row(
    ui: &mut egui::Ui,
    user: &AdminUser,
    snapshot: AdminUsersSnapshot<'_>,
    state: &mut AdminUsersState,
    colors: crate::theme::Palette,
) {
    let is_current = snapshot
        .current_user_id
        .is_some_and(|current_id| same_user_id(current_id, &user.id));
    let current_user_known = current_user_is_known(snapshot);
    egui::Frame::new()
        .fill(colors.panel)
        .stroke(egui::Stroke::new(1.0, colors.divider))
        .corner_radius(egui::CornerRadius::same(12))
        .inner_margin(egui::Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new(&user.name)
                            .size(17.0)
                            .strong()
                            .color(colors.text),
                    );
                    ui.label(RichText::new(&user.id).small().color(colors.muted));
                    ui.horizontal(|ui| {
                        if is_current {
                            ui.label(RichText::new("Signed in").small().color(colors.accent));
                        }
                        if let Some(policy) = user.policy.as_ref() {
                            if policy.is_administrator == Some(true) {
                                ui.label(
                                    RichText::new("Administrator").small().color(colors.accent),
                                );
                            }
                            if policy.is_disabled == Some(true) {
                                ui.label(RichText::new("Disabled").small().color(colors.danger));
                            }
                        } else {
                            ui.label(
                                RichText::new("Policy unavailable")
                                    .small()
                                    .color(colors.muted),
                            );
                        }
                    });
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if is_current {
                        ui.label(RichText::new("Current account").color(colors.muted));
                    } else if current_user_known {
                        if ui
                            .add_enabled(!snapshot.loading, egui::Button::new("Delete"))
                            .clicked()
                        {
                            state.open_delete(user, snapshot);
                        }
                    } else {
                        ui.label(
                            RichText::new("Delete unavailable")
                                .small()
                                .color(colors.muted),
                        );
                    }

                    if ui
                        .add_enabled(!snapshot.loading, egui::Button::new("Permissions"))
                        .clicked()
                    {
                        state.open_policy(user);
                    }
                    if ui
                        .add_enabled(!snapshot.loading, egui::Button::new("Rename"))
                        .clicked()
                    {
                        state.open_rename(user);
                    }
                });
            });
        });
}

fn draw_active_dialog(
    ctx: &egui::Context,
    snapshot: AdminUsersSnapshot<'_>,
    state: &mut AdminUsersState,
    actions: &mut Vec<AdminUsersAction>,
    colors: crate::theme::Palette,
) {
    if state.pending_policy.is_some() {
        draw_policy_confirmation(ctx, snapshot.loading, state, actions, colors);
    } else if state.create_open {
        draw_create_dialog(ctx, snapshot.loading, state, actions, colors);
    } else if state.rename.is_some() {
        draw_rename_dialog(ctx, snapshot.loading, state, actions, colors);
    } else if state.policy.is_some() {
        draw_policy_dialog(ctx, snapshot.loading, state, actions, colors);
    } else if state.delete.is_some() {
        draw_delete_dialog(ctx, snapshot, state, actions, colors);
    }
}

fn draw_create_dialog(
    ctx: &egui::Context,
    busy: bool,
    state: &mut AdminUsersState,
    actions: &mut Vec<AdminUsersAction>,
    colors: crate::theme::Palette,
) {
    let mut submit = false;
    let mut cancel = false;
    let modal = egui::Modal::new(egui::Id::unique("admin-users-create")).show(ctx, |ui| {
        ui.set_width(440.0);
        ui.heading("Create a user");
        ui.add_space(4.0);
        ui.label(RichText::new("Set a name and an initial password.").color(colors.muted));

        ui.add_space(12.0);
        ui.label("User name");
        let name = ui.add(
            egui::TextEdit::singleline(&mut state.create_name)
                .char_limit(MAX_USER_NAME_BYTES)
                .hint_text("Name shown on the server"),
        );
        name.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "User name"));
        if name.changed() {
            state.create_error = None;
        }

        ui.add_space(8.0);
        ui.label("Password");
        let password = ui.add(
            egui::TextEdit::singleline(&mut *state.create_password)
                .password(true)
                .char_limit(MAX_PASSWORD_BYTES)
                .hint_text("Initial password"),
        );
        password.widget_info(|| {
            egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Password".to_owned())
        });

        if let Some(error) = state.create_error {
            ui.add_space(6.0);
            ui.colored_label(colors.danger, error);
        }

        let name_valid = validate_user_name(&state.create_name).is_ok();
        let password_valid =
            !state.create_password.is_empty() && state.create_password.len() <= MAX_PASSWORD_BYTES;
        if !state.create_name.is_empty()
            && let Err(error) = validate_user_name(&state.create_name)
        {
            ui.colored_label(colors.danger, error.message());
        }
        if state.create_password.len() > MAX_PASSWORD_BYTES {
            ui.colored_label(colors.danger, "Passwords must be 4,096 bytes or fewer.");
        }
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            if ui.button("Cancel").clicked() {
                cancel = true;
            }
            if ui
                .add_enabled(
                    !busy && name_valid && password_valid,
                    egui::Button::new("Create user"),
                )
                .clicked()
            {
                submit = true;
            }
        });
    });

    if submit {
        if let Some(action) = state.submit_create() {
            actions.push(action);
        }
    } else if cancel || modal.should_close() {
        state.cancel_create();
    }
}

fn draw_rename_dialog(
    ctx: &egui::Context,
    busy: bool,
    state: &mut AdminUsersState,
    actions: &mut Vec<AdminUsersAction>,
    colors: crate::theme::Palette,
) {
    let mut submit = false;
    let mut cancel = false;
    let Some(draft) = state.rename.as_mut() else {
        return;
    };
    let modal = egui::Modal::new(egui::Id::unique("admin-users-rename")).show(ctx, |ui| {
        ui.set_width(420.0);
        ui.heading("Rename user");
        ui.label(RichText::new(&draft.current_name).color(colors.muted));
        ui.add_space(10.0);
        ui.label("New user name");
        let field = ui.add(
            egui::TextEdit::singleline(&mut draft.name)
                .char_limit(MAX_USER_NAME_BYTES)
                .hint_text("User name"),
        );
        field.widget_info(|| {
            egui::WidgetInfo::labeled(egui::Role::TextInput, true, "New user name")
        });
        if field.changed() {
            draft.error = None;
        }
        if let Some(error) = draft.error {
            ui.colored_label(colors.danger, error);
        }
        if !draft.name.is_empty()
            && let Err(error) = validate_user_name(&draft.name)
        {
            ui.colored_label(colors.danger, error.message());
        }
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if ui.button("Cancel").clicked() {
                cancel = true;
            }
            if ui
                .add_enabled(
                    !busy && validate_user_name(&draft.name).is_ok(),
                    egui::Button::new("Save name"),
                )
                .clicked()
            {
                submit = true;
            }
        });
    });

    if submit {
        let Some(mut draft) = state.rename.take() else {
            return;
        };
        match validate_user_name(&draft.name) {
            Ok(name) => actions.push(AdminUsersAction::Rename {
                user_id: draft.user_id,
                name,
            }),
            Err(error) => {
                draft.error = Some(error.message());
                state.rename = Some(draft);
            }
        }
    } else if cancel || modal.should_close() {
        state.rename = None;
    }
}

fn draw_policy_dialog(
    ctx: &egui::Context,
    busy: bool,
    state: &mut AdminUsersState,
    actions: &mut Vec<AdminUsersAction>,
    colors: crate::theme::Palette,
) {
    let mut save = false;
    let mut cancel = false;
    let Some(draft) = state.policy.as_mut() else {
        return;
    };
    let modal = egui::Modal::new(egui::Id::unique("admin-users-policy")).show(ctx, |ui| {
        ui.set_width(520.0);
        ui.heading("Permissions");
        ui.label(RichText::new(&draft.user_name).color(colors.muted));
        ui.add_space(10.0);
        policy_checkbox(
            ui,
            "Administrator access",
            &mut draft.edited.is_administrator,
        );
        policy_checkbox(ui, "Account disabled", &mut draft.edited.is_disabled);
        policy_checkbox(
            ui,
            "Media playback",
            &mut draft.edited.enable_media_playback,
        );
        policy_checkbox(ui, "Remote access", &mut draft.edited.enable_remote_access);
        policy_checkbox(ui, "Hidden from user lists", &mut draft.edited.is_hidden);
        if needs_privilege_confirmation(&draft.changes()) {
            ui.add_space(6.0);
            ui.colored_label(
                colors.warning,
                "Administrator and disabled-status changes need a second confirmation.",
            );
        }
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if ui.button("Cancel").clicked() {
                cancel = true;
            }
            if ui
                .add_enabled(
                    !busy && !policy_changes_empty(&draft.changes()),
                    egui::Button::new("Save permissions"),
                )
                .clicked()
            {
                save = true;
            }
        });
    });

    if save {
        state.stage_policy_patch(actions);
    } else if cancel || modal.should_close() {
        state.policy = None;
    }
}

fn policy_checkbox(ui: &mut egui::Ui, label: &str, value: &mut Option<bool>) {
    if let Some(value) = value.as_mut() {
        ui.checkbox(value, label);
    } else {
        ui.horizontal(|ui| {
            ui.label(label);
            ui.label(
                RichText::new("Unavailable")
                    .small()
                    .color(ui.visuals().weak_text_color()),
            );
        });
    }
}

fn draw_policy_confirmation(
    ctx: &egui::Context,
    busy: bool,
    state: &mut AdminUsersState,
    actions: &mut Vec<AdminUsersAction>,
    colors: crate::theme::Palette,
) {
    let mut confirmed = false;
    let mut canceled = false;
    let Some(pending) = state.pending_policy.as_ref() else {
        return;
    };
    let modal = egui::Modal::new(egui::Id::unique("admin-users-policy-confirm")).show(ctx, |ui| {
        ui.set_width(440.0);
        ui.heading("Confirm access change");
        ui.label(format!(
            "You are changing administrator or disabled status for {}.",
            pending.user_name
        ));
        ui.add_space(8.0);
        if let Some(value) = pending.changes.is_administrator {
            ui.label(format!(
                "Administrator access: {}",
                if value { "enabled" } else { "removed" }
            ));
        }
        if let Some(value) = pending.changes.is_disabled {
            ui.label(format!(
                "Account status: {}",
                if value { "disabled" } else { "enabled" }
            ));
        }
        ui.add_space(8.0);
        ui.colored_label(
            colors.warning,
            "This may change who can sign in or manage the server.",
        );
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if ui.button("Keep editing").clicked() {
                canceled = true;
            }
            if ui
                .add_enabled(
                    !busy,
                    egui::Button::new("Confirm change").fill(colors.danger),
                )
                .clicked()
            {
                confirmed = true;
            }
        });
    });

    if confirmed {
        state.resolve_policy_confirmation(true, actions);
    } else if canceled || modal.should_close() {
        state.resolve_policy_confirmation(false, actions);
    }
}

fn draw_delete_dialog(
    ctx: &egui::Context,
    snapshot: AdminUsersSnapshot<'_>,
    state: &mut AdminUsersState,
    actions: &mut Vec<AdminUsersAction>,
    colors: crate::theme::Palette,
) {
    if !state.delete_target_is_present(snapshot) {
        state.delete = None;
        return;
    }

    let mut confirmed = false;
    let mut canceled = false;
    let Some(draft) = state.delete.as_mut() else {
        return;
    };
    let modal = egui::Modal::new(egui::Id::unique("admin-users-delete")).show(ctx, |ui| {
        ui.set_width(440.0);
        ui.heading("Delete user");
        ui.label(format!(
            "This permanently removes the account for {}.",
            draft.target_name
        ));
        ui.add_space(8.0);
        ui.label(format!("Type {} to confirm.", draft.target_name));
        let confirmation = ui.add(
            egui::TextEdit::singleline(&mut draft.typed_name)
                .char_limit(draft.target_name.chars().count().max(1))
                .hint_text("Exact user name"),
        );
        confirmation.widget_info(|| {
            egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Confirm user name")
        });
        ui.add_space(12.0);
        let ready = delete_confirmation_ready(snapshot, draft);
        ui.horizontal(|ui| {
            if ui.button("Cancel").clicked() {
                canceled = true;
            }
            if ui
                .add_enabled(ready, egui::Button::new("Delete user").fill(colors.danger))
                .clicked()
            {
                confirmed = true;
            }
        });
    });

    if confirmed {
        if let Some(action) = state.confirm_delete(snapshot) {
            actions.push(action);
        }
    } else if canceled || modal.should_close() {
        state.delete = None;
    }
}

fn validate_user_name(value: &str) -> Result<String, NameError> {
    let name = value.trim();
    if name.is_empty() {
        return Err(NameError::Blank);
    }
    if name.len() > MAX_USER_NAME_BYTES {
        return Err(NameError::TooLong);
    }
    if name.chars().any(char::is_control) {
        return Err(NameError::ControlCharacter);
    }
    Ok(name.to_owned())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NameError {
    Blank,
    TooLong,
    ControlCharacter,
}

impl NameError {
    fn message(self) -> &'static str {
        match self {
            Self::Blank => "Enter a user name.",
            Self::TooLong => "User names must be 256 bytes or fewer.",
            Self::ControlCharacter => "User names cannot contain control characters.",
        }
    }
}

fn changed_value(original: Option<bool>, edited: Option<bool>) -> Option<bool> {
    (original != edited).then_some(edited).flatten()
}

fn policy_changes_empty(changes: &AdminUserPolicyChanges) -> bool {
    changes.is_administrator.is_none()
        && changes.is_hidden.is_none()
        && changes.is_disabled.is_none()
        && changes.enable_media_playback.is_none()
        && changes.enable_remote_access.is_none()
}

fn needs_privilege_confirmation(changes: &AdminUserPolicyChanges) -> bool {
    changes.is_administrator.is_some() || changes.is_disabled.is_some()
}

fn same_user_id(left: &str, right: &str) -> bool {
    left.replace('-', "")
        .eq_ignore_ascii_case(&right.replace('-', ""))
}

fn current_user_is_known(snapshot: AdminUsersSnapshot<'_>) -> bool {
    snapshot.current_user_id.is_some_and(|current_id| {
        snapshot
            .users
            .iter()
            .any(|user| same_user_id(current_id, &user.id))
    })
}

fn delete_confirmation_ready(snapshot: AdminUsersSnapshot<'_>, draft: &DeleteDraft) -> bool {
    if snapshot.loading {
        return false;
    }
    let Some(current_user_id) = snapshot.current_user_id else {
        return false;
    };
    if !current_user_is_known(snapshot)
        || same_user_id(current_user_id, &draft.user_id)
        || draft.typed_name != draft.target_name
    {
        return false;
    }
    snapshot
        .users
        .iter()
        .any(|user| same_user_id(&user.id, &draft.user_id) && user.name == draft.target_name)
}

impl AdminUsersState {
    fn delete_target_is_present(&self, snapshot: AdminUsersSnapshot<'_>) -> bool {
        let Some(draft) = self.delete.as_ref() else {
            return false;
        };
        current_user_is_known(snapshot)
            && snapshot.current_user_id.is_some_and(|current_id| {
                !same_user_id(current_id, &draft.user_id)
                    && snapshot.users.iter().any(|user| {
                        same_user_id(&user.id, &draft.user_id) && user.name == draft.target_name
                    })
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_inputs_have_accessible_names_without_exposing_the_password() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut state = AdminUsersState::default();
        state.open_create();
        state.create_password = Zeroizing::new("fixture-private-password".to_owned());
        let mut tree = None;
        for frame in 0..2 {
            let mut actions = Vec::new();
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(800.0, 600.0),
                    )),
                    time: Some(f64::from(frame) / 60.0),
                    ..Default::default()
                },
                |ui| actions.extend(show(ui, snapshot(&[], None), &mut state)),
            );
            // The first modal frame sizes its area. No GPU upload is needed in this test.
            output.textures_delta.clear();
            assert!(actions.is_empty());
            tree = output.platform_output.accesskit_update;
        }
        let tree = tree.expect("accessible form after the modal sizing frame");
        for label in ["User name", "Password"] {
            assert!(
                tree.nodes.iter().any(|(_, node)| {
                    node.role() == egui::accesskit::Role::TextInput && node.label() == Some(label)
                }),
                "missing named input: {label}; nodes={:?}",
                tree.nodes
                    .iter()
                    .map(|(_, node)| (node.role(), node.label()))
                    .collect::<Vec<_>>()
            );
        }
        assert!(!format!("{tree:?}").contains("fixture-private-password"));
    }

    fn user(id: &str, name: &str, policy: AdminUserPolicy) -> AdminUser {
        AdminUser {
            id: id.to_owned(),
            name: name.to_owned(),
            policy: Some(policy),
        }
    }

    fn snapshot<'a>(users: &'a [AdminUser], current_id: Option<&'a str>) -> AdminUsersSnapshot<'a> {
        AdminUsersSnapshot {
            users,
            loading: false,
            error: None,
            current_user_id: current_id,
        }
    }

    #[test]
    fn opening_component_state_does_not_emit_a_mutation() {
        let mut state = AdminUsersState::default();
        state.open_create();
        assert!(state.create_open);
        assert!(state.submit_create().is_none());
        assert!(state.policy.is_none());
        assert!(state.delete.is_none());
    }

    #[test]
    fn create_submission_moves_password_out_and_redacts_debug() {
        let mut state = AdminUsersState::default();
        state.open_create();
        state.create_name = "  Riley  ".to_owned();
        state.create_password = Zeroizing::new("component-test-secret".to_owned());

        let action = state.submit_create().expect("valid create draft");
        assert!(state.create_password.is_empty());
        assert!(!state.create_open);
        assert!(!format!("{state:?}").contains("component-test-secret"));
        assert!(!format!("{action:?}").contains("component-test-secret"));
        match action {
            AdminUsersAction::Create { name, password } => {
                assert_eq!(name, "Riley");
                assert_eq!(password.as_str(), "component-test-secret");
            }
            _ => panic!("expected create action"),
        }
    }

    #[test]
    fn cancel_zeroizes_an_unsent_password_draft() {
        let mut state = AdminUsersState::default();
        state.open_create();
        state.create_name = "Riley".to_owned();
        state.create_password = Zeroizing::new("cancel-me".to_owned());

        state.cancel();

        assert!(state.create_password.is_empty());
        assert!(state.create_name.is_empty());
        assert!(!state.create_open);
    }

    #[test]
    fn deletion_requires_exact_target_name_and_never_allows_current_user() {
        let accounts = [
            user(
                "0000000000000000000000000000a001",
                "Admin",
                AdminUserPolicy::default(),
            ),
            user(
                "0000000000000000000000000000a002",
                "Riley",
                AdminUserPolicy::default(),
            ),
        ];
        let view = snapshot(&accounts, Some("0000000000000000000000000000a001"));
        let mut state = AdminUsersState::default();

        state.open_delete(&accounts[1], view);
        assert!(!state.delete_confirmation_ready(view));
        if let Some(draft) = state.delete.as_mut() {
            draft.typed_name = "riley".to_owned();
        }
        assert!(!state.delete_confirmation_ready(view));
        if let Some(draft) = state.delete.as_mut() {
            draft.typed_name = "Riley".to_owned();
        }
        assert!(state.delete_confirmation_ready(view));
        assert!(matches!(
            state.confirm_delete(view),
            Some(AdminUsersAction::Delete { user_id }) if user_id == accounts[1].id
        ));

        state.open_delete(&accounts[0], view);
        assert!(state.delete.is_none());
    }

    #[test]
    fn administrator_or_disabled_changes_wait_for_explicit_confirmation() {
        let mut policy = AdminUserPolicy::default();
        policy.is_administrator = Some(false);
        policy.is_disabled = Some(false);
        policy.enable_media_playback = Some(true);
        let account = user("user-id", "Riley", policy);
        let mut state = AdminUsersState::default();
        state.open_policy(&account);
        if let Some(draft) = state.policy.as_mut() {
            draft.edited.is_administrator = Some(true);
            draft.edited.enable_media_playback = Some(false);
        }

        let mut actions = Vec::new();
        state.stage_policy_patch(&mut actions);
        assert!(actions.is_empty());
        assert!(state.pending_policy.is_some());

        state.resolve_policy_confirmation(false, &mut actions);
        assert!(actions.is_empty());
        assert!(state.policy.is_some(), "cancel returns to editing");
        if let Some(draft) = state.policy.as_mut() {
            draft.edited.is_administrator = Some(true);
        }
        state.stage_policy_patch(&mut actions);
        state.resolve_policy_confirmation(true, &mut actions);
        assert!(matches!(
            actions.as_slice(),
            [AdminUsersAction::PolicyPatch { changes, .. }]
                if changes.is_administrator == Some(true)
                    && changes.enable_media_playback == Some(false)
        ));
    }

    #[test]
    fn ordinary_policy_changes_emit_only_after_save_without_privilege_prompt() {
        let mut policy = AdminUserPolicy::default();
        policy.is_hidden = Some(false);
        policy.enable_remote_access = Some(true);
        let account = user("user-id", "Riley", policy);
        let mut state = AdminUsersState::default();
        state.open_policy(&account);
        if let Some(draft) = state.policy.as_mut() {
            draft.edited.is_hidden = Some(true);
        }

        let mut actions = Vec::new();
        state.stage_policy_patch(&mut actions);
        assert!(state.pending_policy.is_none());
        assert!(matches!(
            actions.as_slice(),
            [AdminUsersAction::PolicyPatch { changes, .. }]
                if changes.is_hidden == Some(true)
                    && changes.enable_remote_access.is_none()
        ));
    }
}

//! Administrator dialogs for one item, modelled on Jellyfin Web's metadata editor
//! (`src/components/metadataEditor`), item identifier (`src/components/itemidentifier`) and
//! image editor (`src/components/imageeditor`, `src/components/imageDownloader`).
//!
//! The dialog owns no channel: [`MetadataEditor::show`] and [`MetadataEditor::on_reply`]
//! return the worker requests to send, and the app routes the replies back.

use std::collections::HashMap;

use jellyfin_api::{
    EditableItem, ErrorClass, ImageType, ItemImage, LOCKABLE_FIELDS, MetadataFields, RemoteImage,
    RemoteSearchQuery, RemoteSearchResult, is_identifiable, is_iso_date,
};
use kattri_i18n::ids;

use super::{ui_text, ui_text_args};
use crate::worker::{MetadataOp, MetadataReply, ThumbnailSource};

/// Which dialog is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EditorMode {
    Edit,
    Identify,
    Images,
}

/// Work the dialog asks the app to send to the worker.
#[derive(Clone, Debug)]
pub(crate) enum EditorRequest {
    Op(MetadataOp),
    Thumbnail {
        key: String,
        source: ThumbnailSource,
    },
}

/// What the app does after a frame or a reply.
#[derive(Debug, Default)]
pub(crate) struct EditorOutcome {
    pub requests: Vec<EditorRequest>,
    /// Close the dialog; show this toast when set.
    pub close: Option<Option<String>>,
}

impl EditorOutcome {
    fn request(op: MetadataOp) -> Self {
        Self {
            requests: vec![EditorRequest::Op(op)],
            close: None,
        }
    }
}

/// The text form of [`MetadataFields`], so half-typed numbers and dates can be shown.
#[derive(Clone, Debug, Default, PartialEq)]
struct Draft {
    name: String,
    sort_name: String,
    original_title: String,
    overview: String,
    year: String,
    premiere_date: String,
    official_rating: String,
    community_rating: String,
    genres: String,
    tags: String,
    studios: String,
    lock_data: bool,
    locked_fields: Vec<String>,
}

impl Draft {
    fn from_fields(fields: &MetadataFields) -> Self {
        Self {
            name: fields.name.clone(),
            sort_name: fields.sort_name.clone(),
            original_title: fields.original_title.clone(),
            overview: fields.overview.clone(),
            year: fields
                .production_year
                .map(|year| year.to_string())
                .unwrap_or_default(),
            premiere_date: fields.premiere_date.clone(),
            official_rating: fields.official_rating.clone(),
            community_rating: fields
                .community_rating
                .map(|rating| format!("{rating:.1}"))
                .unwrap_or_default(),
            genres: fields.genres.join(", "),
            tags: fields.tags.join(", "),
            studios: fields.studios.join(", "),
            lock_data: fields.lock_data,
            locked_fields: fields.locked_fields.clone(),
        }
    }

    /// The fields to save, or the message id of the first invalid entry.
    fn to_fields(&self) -> Result<MetadataFields, &'static str> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err(ids::METADATA_ERROR_NAME_REQUIRED);
        }
        let year = match self.year.trim() {
            "" => None,
            year => Some(
                year.parse::<i32>()
                    .ok()
                    .filter(|year| (1..=9999).contains(year))
                    .ok_or(ids::METADATA_ERROR_YEAR)?,
            ),
        };
        let premiere_date = self.premiere_date.trim();
        if !premiere_date.is_empty() && !is_iso_date(premiere_date) {
            return Err(ids::METADATA_ERROR_DATE);
        }
        let community_rating = match self.community_rating.trim() {
            "" => None,
            rating => Some(
                rating
                    .replace(',', ".")
                    .parse::<f32>()
                    .ok()
                    .filter(|rating| (0.0..=10.0).contains(rating))
                    .ok_or(ids::METADATA_ERROR_RATING)?,
            ),
        };
        Ok(MetadataFields {
            name: name.to_owned(),
            sort_name: self.sort_name.trim().to_owned(),
            original_title: self.original_title.trim().to_owned(),
            overview: self.overview.trim().to_owned(),
            production_year: year,
            premiere_date: premiere_date.to_owned(),
            official_rating: self.official_rating.trim().to_owned(),
            community_rating,
            genres: split_list(&self.genres),
            tags: split_list(&self.tags),
            studios: split_list(&self.studios),
            lock_data: self.lock_data,
            locked_fields: self.locked_fields.clone(),
        })
    }
}

fn split_list(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
        .collect()
}

/// One open administrator dialog.
pub(crate) struct MetadataEditor {
    pub item_id: String,
    pub title: String,
    pub mode: EditorMode,
    busy: bool,
    error: Option<&'static str>,
    item: Option<EditableItem>,
    draft: Draft,
    search_name: String,
    search_year: String,
    search_ids: Vec<(String, String)>,
    results: Option<Vec<RemoteSearchResult>>,
    replace_images: bool,
    images: Vec<ItemImage>,
    remote_type: ImageType,
    all_languages: bool,
    remote: Option<Vec<RemoteImage>>,
    confirm_delete: Option<ItemImage>,
    /// Loaded previews by key; `None` while one is on its way or failed.
    thumbnails: HashMap<String, Option<egui::TextureHandle>>,
}

impl MetadataEditor {
    /// Opens `mode` for an item and returns the first request to send.
    pub(crate) fn open(item_id: String, title: String, mode: EditorMode) -> (Self, EditorOutcome) {
        let editor = Self {
            item_id,
            title,
            mode,
            busy: true,
            error: None,
            item: None,
            draft: Draft::default(),
            search_name: String::new(),
            search_year: String::new(),
            search_ids: Vec::new(),
            results: None,
            replace_images: false,
            images: Vec::new(),
            remote_type: ImageType::Primary,
            all_languages: false,
            remote: None,
            confirm_delete: None,
            thumbnails: HashMap::new(),
        };
        let first = match mode {
            EditorMode::Edit | EditorMode::Identify => MetadataOp::Load,
            EditorMode::Images => MetadataOp::ListImages,
        };
        (editor, EditorOutcome::request(first))
    }

    /// Applies a worker reply for this dialog.
    pub(crate) fn on_reply(&mut self, reply: Result<MetadataReply, ErrorClass>) -> EditorOutcome {
        self.busy = false;
        let reply = match reply {
            Ok(reply) => reply,
            Err(class) => {
                self.error = Some(match class {
                    ErrorClass::Forbidden => ids::METADATA_ERROR_FORBIDDEN,
                    _ => ids::METADATA_ERROR_FAILED,
                });
                return EditorOutcome::default();
            }
        };
        self.error = None;
        let title = [("title", self.title.as_str())];
        match reply {
            MetadataReply::Loaded(item) => {
                self.draft = Draft::from_fields(&item.fields);
                self.search_name = item.fields.name.clone();
                self.search_year = item
                    .fields
                    .production_year
                    .map(|year| year.to_string())
                    .unwrap_or_default();
                self.search_ids = item.provider_ids.clone();
                self.item = Some(*item);
                EditorOutcome::default()
            }
            MetadataReply::Saved => EditorOutcome {
                requests: Vec::new(),
                close: Some(Some(ui_text_args(ids::TOAST_METADATA_SAVED, &title))),
            },
            MetadataReply::Applied => EditorOutcome {
                requests: Vec::new(),
                close: Some(Some(ui_text_args(ids::TOAST_IDENTIFY_APPLIED, &title))),
            },
            MetadataReply::SearchResults(results) => {
                let requests = results
                    .iter()
                    .filter_map(|result| result.image_url.clone())
                    .filter_map(|url| self.thumbnail_request(ThumbnailSource::Remote(url)))
                    .collect();
                self.results = Some(results);
                EditorOutcome {
                    requests,
                    close: None,
                }
            }
            MetadataReply::Images(images) => {
                let item_id = self.item_id.clone();
                let requests = images
                    .iter()
                    .filter_map(|image| {
                        self.thumbnail_request(ThumbnailSource::Item {
                            item_id: item_id.clone(),
                            image: image.clone(),
                        })
                    })
                    .collect();
                self.images = images;
                EditorOutcome {
                    requests,
                    close: None,
                }
            }
            MetadataReply::RemoteImages(remote) => {
                let requests = remote
                    .iter()
                    .map(|image| image.thumbnail_url.clone().unwrap_or(image.url.clone()))
                    .filter_map(|url| self.thumbnail_request(ThumbnailSource::Remote(url)))
                    .collect();
                self.remote = Some(remote);
                EditorOutcome {
                    requests,
                    close: None,
                }
            }
            MetadataReply::ImagesChanged => {
                self.busy = true;
                EditorOutcome::request(MetadataOp::ListImages)
            }
        }
    }

    /// Stores a decoded preview for `key`.
    pub(crate) fn thumbnail_loaded(&mut self, key: String, texture: egui::TextureHandle) {
        self.thumbnails.insert(key, Some(texture));
    }

    fn thumbnail_request(&mut self, source: ThumbnailSource) -> Option<EditorRequest> {
        let key = thumbnail_key(&source);
        if self.thumbnails.contains_key(&key) {
            return None;
        }
        self.thumbnails.insert(key.clone(), None);
        Some(EditorRequest::Thumbnail { key, source })
    }

    /// Draws the dialog for one frame.
    pub(crate) fn show(&mut self, ctx: &egui::Context) -> EditorOutcome {
        let mut outcome = EditorOutcome::default();
        let heading_id = match self.mode {
            EditorMode::Edit => ids::METADATA_EDIT_TITLE,
            EditorMode::Identify => ids::METADATA_IDENTIFY_TITLE,
            EditorMode::Images => ids::METADATA_IMAGES_TITLE,
        };
        let heading = ui_text_args(heading_id, &[("title", self.title.as_str())]);
        let modal = egui::Modal::new(egui::Id::unique("kattri-metadata-editor")).show(ctx, |ui| {
            ui.set_width(ui.available_width().clamp(320.0, 720.0));
            ui.heading(heading);
            if self.busy {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(ui_text(ids::METADATA_LOADING));
                });
            }
            if let Some(error) = self.error {
                ui.colored_label(ui.visuals().error_fg_color, ui_text(error));
            }
            ui.separator();
            let max_height = (ui.ctx().content_rect().height() - 220.0).max(200.0);
            egui::ScrollArea::vertical()
                .max_height(max_height)
                .auto_shrink([false, true])
                .show(ui, |ui| match self.mode {
                    EditorMode::Edit => self.edit_form(ui),
                    EditorMode::Identify => self.identify_form(ui, &mut outcome),
                    EditorMode::Images => self.images_form(ui, &mut outcome),
                });
            ui.separator();
            ui.horizontal(|ui| {
                if self.mode == EditorMode::Edit
                    && self.item.is_some()
                    && ui
                        .add_enabled(!self.busy, egui::Button::new(ui_text(ids::METADATA_SAVE)))
                        .clicked()
                {
                    match self.draft.to_fields() {
                        Ok(fields) => {
                            if let Some(item) = self.item.clone() {
                                self.busy = true;
                                self.error = None;
                                outcome.requests.push(EditorRequest::Op(MetadataOp::Save {
                                    item: Box::new(item),
                                    fields: Box::new(fields),
                                }));
                            }
                        }
                        Err(message) => self.error = Some(message),
                    }
                }
                let close_label = if self.mode == EditorMode::Edit {
                    ids::METADATA_CANCEL
                } else {
                    ids::METADATA_CLOSE
                };
                if ui.button(ui_text(close_label)).clicked() {
                    outcome.close = Some(None);
                }
            });
        });
        if modal.should_close() && outcome.close.is_none() && self.confirm_delete.is_none() {
            outcome.close = Some(None);
        }
        self.delete_confirmation(ctx, &mut outcome);
        outcome
    }

    fn edit_form(&mut self, ui: &mut egui::Ui) {
        let Some(item) = self.item.as_ref() else {
            return;
        };
        let people = item.people.clone();
        let draft = &mut self.draft;
        egui::Grid::new("kattri-metadata-fields")
            .num_columns(2)
            .spacing([12.0, 8.0])
            .show(ui, |ui| {
                text_row(ui, ids::METADATA_FIELD_NAME, &mut draft.name);
                text_row(ui, ids::METADATA_FIELD_SORT_NAME, &mut draft.sort_name);
                text_row(
                    ui,
                    ids::METADATA_FIELD_ORIGINAL_TITLE,
                    &mut draft.original_title,
                );
                text_row(ui, ids::METADATA_FIELD_YEAR, &mut draft.year);
                text_row(
                    ui,
                    ids::METADATA_FIELD_PREMIERE_DATE,
                    &mut draft.premiere_date,
                );
                text_row(
                    ui,
                    ids::METADATA_FIELD_OFFICIAL_RATING,
                    &mut draft.official_rating,
                );
                text_row(
                    ui,
                    ids::METADATA_FIELD_COMMUNITY_RATING,
                    &mut draft.community_rating,
                );
                text_row(ui, ids::METADATA_FIELD_GENRES, &mut draft.genres);
                text_row(ui, ids::METADATA_FIELD_TAGS, &mut draft.tags);
                text_row(ui, ids::METADATA_FIELD_STUDIOS, &mut draft.studios);
            });
        ui.small(ui_text(ids::METADATA_LIST_HINT));
        ui.add_space(8.0);
        ui.label(ui_text(ids::METADATA_FIELD_OVERVIEW));
        ui.add(
            egui::TextEdit::multiline(&mut draft.overview)
                .desired_rows(5)
                .desired_width(f32::INFINITY),
        );
        ui.add_space(8.0);
        ui.label(egui::RichText::new(ui_text(ids::METADATA_FIELD_PEOPLE)).strong());
        if people.is_empty() {
            ui.label(ui_text(ids::METADATA_PEOPLE_EMPTY));
        }
        for person in people.iter().take(200) {
            let name = person.name.as_deref().unwrap_or_default();
            let detail = [person.person_type.as_deref(), person.role.as_deref()]
                .into_iter()
                .flatten()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join(" · ");
            if detail.is_empty() {
                ui.label(name);
            } else {
                ui.label(format!("{name} — {detail}"));
            }
        }
        ui.add_space(8.0);
        ui.checkbox(&mut draft.lock_data, ui_text(ids::METADATA_LOCK_DATA));
        ui.label(egui::RichText::new(ui_text(ids::METADATA_LOCKED_FIELDS)).strong());
        ui.horizontal_wrapped(|ui| {
            for &field in LOCKABLE_FIELDS {
                let mut locked = draft.locked_fields.iter().any(|locked| locked == field);
                if ui
                    .checkbox(&mut locked, ui_text(lock_field_label(field)))
                    .changed()
                {
                    draft.locked_fields.retain(|existing| existing != field);
                    if locked {
                        draft.locked_fields.push(field.to_owned());
                    }
                }
            }
        });
    }

    fn identify_form(&mut self, ui: &mut egui::Ui, outcome: &mut EditorOutcome) {
        let Some(item) = self.item.as_ref() else {
            return;
        };
        let item_type = item.item_type.clone().unwrap_or_default();
        if !is_identifiable(&item_type) {
            ui.label(ui_text(ids::METADATA_NOT_IDENTIFIABLE));
            return;
        }
        egui::Grid::new("kattri-identify-fields")
            .num_columns(2)
            .spacing([12.0, 8.0])
            .show(ui, |ui| {
                text_row(ui, ids::METADATA_FIELD_NAME, &mut self.search_name);
                text_row(ui, ids::METADATA_FIELD_YEAR, &mut self.search_year);
                for (provider, value) in &mut self.search_ids {
                    ui.label(provider.as_str());
                    ui.add(egui::TextEdit::singleline(value).desired_width(320.0));
                    ui.end_row();
                }
            });
        if ui
            .add_enabled(!self.busy, egui::Button::new(ui_text(ids::METADATA_SEARCH)))
            .clicked()
        {
            let year = self.search_year.trim().parse::<i32>().ok();
            let query = RemoteSearchQuery {
                name: self.search_name.trim().to_owned(),
                year,
                provider_ids: self.search_ids.clone(),
            };
            let has_id = query
                .provider_ids
                .iter()
                .any(|(_, value)| !value.trim().is_empty());
            if query.name.is_empty() && !has_id {
                self.error = Some(ids::METADATA_ERROR_NAME_OR_ID);
            } else {
                self.busy = true;
                self.error = None;
                outcome
                    .requests
                    .push(EditorRequest::Op(MetadataOp::Search { item_type, query }));
            }
        }
        let Some(results) = self.results.as_ref() else {
            return;
        };
        ui.add_space(8.0);
        if results.is_empty() {
            ui.label(ui_text(ids::METADATA_NO_RESULTS));
            return;
        }
        ui.checkbox(
            &mut self.replace_images,
            ui_text(ids::METADATA_REPLACE_IMAGES),
        );
        let mut chosen = None;
        for (index, result) in results.iter().enumerate() {
            ui.separator();
            ui.horizontal(|ui| {
                let key = result
                    .image_url
                    .as_ref()
                    .map(|url| thumbnail_key(&ThumbnailSource::Remote(url.clone())));
                preview(ui, key.and_then(|key| self.thumbnails.get(&key)), 80.0);
                ui.vertical(|ui| {
                    let mut heading = result.name.clone();
                    if let Some(year) = result.production_year {
                        heading.push_str(&format!(" ({year})"));
                    }
                    ui.label(egui::RichText::new(heading).strong());
                    if let Some(provider) = &result.provider_name {
                        ui.small(provider);
                    }
                    if let Some(overview) = &result.overview {
                        ui.label(truncate(overview, 280));
                    }
                    if ui
                        .add_enabled(!self.busy, egui::Button::new(ui_text(ids::METADATA_APPLY)))
                        .clicked()
                    {
                        chosen = Some(index);
                    }
                });
            });
        }
        if let Some(result) = chosen.and_then(|index| results.get(index)) {
            self.busy = true;
            outcome.requests.push(EditorRequest::Op(MetadataOp::Apply {
                result: Box::new(result.clone()),
                replace_images: self.replace_images,
            }));
        }
    }

    fn images_form(&mut self, ui: &mut egui::Ui, outcome: &mut EditorOutcome) {
        ui.label(egui::RichText::new(ui_text(ids::METADATA_CURRENT_IMAGES)).strong());
        if self.images.is_empty() && !self.busy {
            ui.label(ui_text(ids::METADATA_NO_IMAGES));
        }
        ui.horizontal_wrapped(|ui| {
            for image in &self.images {
                let key = thumbnail_key(&ThumbnailSource::Item {
                    item_id: self.item_id.clone(),
                    image: image.clone(),
                });
                ui.vertical(|ui| {
                    ui.set_width(130.0);
                    preview(ui, self.thumbnails.get(&key), 120.0);
                    ui.small(image_caption(
                        image.image_type,
                        image.index,
                        image.width,
                        image.height,
                    ));
                    if ui
                        .add_enabled(
                            !self.busy,
                            egui::Button::new(ui_text(ids::METADATA_DELETE_IMAGE)),
                        )
                        .clicked()
                    {
                        self.confirm_delete = Some(image.clone());
                    }
                });
            }
        });
        ui.add_space(12.0);
        ui.label(egui::RichText::new(ui_text(ids::METADATA_BROWSE_REMOTE)).strong());
        ui.horizontal(|ui| {
            ui.label(ui_text(ids::METADATA_IMAGE_TYPE));
            egui::ComboBox::from_id_salt("kattri-remote-image-type")
                .selected_text(self.remote_type.as_str())
                .show_ui(ui, |ui| {
                    for &kind in ImageType::ALL {
                        ui.selectable_value(&mut self.remote_type, kind, kind.as_str());
                    }
                });
            ui.checkbox(
                &mut self.all_languages,
                ui_text(ids::METADATA_ALL_LANGUAGES),
            );
            if ui
                .add_enabled(!self.busy, egui::Button::new(ui_text(ids::METADATA_SEARCH)))
                .clicked()
            {
                self.busy = true;
                self.error = None;
                outcome
                    .requests
                    .push(EditorRequest::Op(MetadataOp::RemoteImages {
                        image_type: self.remote_type,
                        all_languages: self.all_languages,
                    }));
            }
        });
        let Some(remote) = self.remote.as_ref() else {
            return;
        };
        if remote.is_empty() {
            ui.label(ui_text(ids::METADATA_REMOTE_EMPTY));
            return;
        }
        let mut chosen = None;
        ui.horizontal_wrapped(|ui| {
            for (index, image) in remote.iter().enumerate() {
                let url = image.thumbnail_url.clone().unwrap_or(image.url.clone());
                let key = thumbnail_key(&ThumbnailSource::Remote(url));
                ui.vertical(|ui| {
                    ui.set_width(130.0);
                    preview(ui, self.thumbnails.get(&key), 120.0);
                    let mut caption = image.provider_name.clone().unwrap_or_default();
                    if let (Some(width), Some(height)) = (image.width, image.height) {
                        caption.push_str(&format!(" · {width} × {height}"));
                    }
                    if let Some(language) = &image.language {
                        caption.push_str(&format!(" · {language}"));
                    }
                    ui.small(caption);
                    if ui
                        .add_enabled(
                            !self.busy,
                            egui::Button::new(ui_text(ids::METADATA_DOWNLOAD)),
                        )
                        .clicked()
                    {
                        chosen = Some(index);
                    }
                });
            }
        });
        if let Some(image) = chosen.and_then(|index| remote.get(index)) {
            self.busy = true;
            outcome
                .requests
                .push(EditorRequest::Op(MetadataOp::Download {
                    image_type: self.remote_type,
                    url: image.url.clone(),
                }));
        }
    }

    fn delete_confirmation(&mut self, ctx: &egui::Context, outcome: &mut EditorOutcome) {
        let Some(image) = self.confirm_delete.clone() else {
            return;
        };
        let mut choice = None;
        let modal =
            egui::Modal::new(egui::Id::unique("kattri-confirm-image-delete")).show(ctx, |ui| {
                ui.set_max_width(380.0);
                ui.label(ui_text_args(
                    ids::METADATA_DELETE_IMAGE_CONFIRM,
                    &[("type", image.image_type.as_str())],
                ));
                ui.horizontal(|ui| {
                    if ui.button(ui_text(ids::METADATA_DELETE_IMAGE)).clicked() {
                        choice = Some(true);
                    }
                    if ui.button(ui_text(ids::METADATA_CANCEL)).clicked() {
                        choice = Some(false);
                    }
                });
            });
        if modal.should_close() && choice.is_none() {
            choice = Some(false);
        }
        match choice {
            Some(true) => {
                self.confirm_delete = None;
                self.busy = true;
                outcome
                    .requests
                    .push(EditorRequest::Op(MetadataOp::DeleteImage {
                        image_type: image.image_type,
                        index: image.index,
                    }));
            }
            Some(false) => self.confirm_delete = None,
            None => {}
        }
    }
}

fn text_row(ui: &mut egui::Ui, label: &str, value: &mut String) {
    ui.label(ui_text(label));
    ui.add(egui::TextEdit::singleline(value).desired_width(360.0));
    ui.end_row();
}

fn preview(ui: &mut egui::Ui, texture: Option<&Option<egui::TextureHandle>>, size: f32) {
    match texture {
        Some(Some(texture)) => {
            let texture_size = texture.size_vec2();
            let scale = (size / texture_size.x.max(texture_size.y)).min(1.0);
            ui.add(egui::Image::new(egui::load::SizedTexture::new(
                texture.id(),
                texture_size * scale,
            )));
        }
        _ => {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
            ui.painter()
                .rect_filled(rect, 6.0, ui.visuals().faint_bg_color);
        }
    }
}

fn image_caption(kind: ImageType, index: u32, width: Option<u32>, height: Option<u32>) -> String {
    let mut caption = if index == 0 {
        kind.as_str().to_owned()
    } else {
        format!("{} {}", kind.as_str(), index + 1)
    };
    if let (Some(width), Some(height)) = (width, height) {
        caption.push_str(&format!(" · {width} × {height}"));
    }
    caption
}

fn truncate(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((index, _)) => format!("{}…", &text[..index]),
        None => text.to_owned(),
    }
}

fn lock_field_label(field: &str) -> &'static str {
    match field {
        "Name" => ids::METADATA_FIELD_NAME,
        "Overview" => ids::METADATA_FIELD_OVERVIEW,
        "Genres" => ids::METADATA_FIELD_GENRES,
        "OfficialRating" => ids::METADATA_FIELD_OFFICIAL_RATING,
        "Cast" => ids::METADATA_FIELD_PEOPLE,
        "ProductionLocations" => ids::METADATA_LOCK_PRODUCTION_LOCATIONS,
        "Runtime" => ids::METADATA_LOCK_RUNTIME,
        "Studios" => ids::METADATA_FIELD_STUDIOS,
        _ => ids::METADATA_FIELD_TAGS,
    }
}

/// Cache key for one preview.
pub(crate) fn thumbnail_key(source: &ThumbnailSource) -> String {
    match source {
        ThumbnailSource::Item { item_id, image } => format!(
            "item:{item_id}:{}:{}:{}",
            image.image_type.as_str(),
            image.index,
            image.tag.as_deref().unwrap_or_default()
        ),
        ThumbnailSource::Remote(url) => format!("remote:{url}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loaded_editor(mode: EditorMode) -> MetadataEditor {
        let (mut editor, outcome) = MetadataEditor::open("m1".into(), "Film".into(), mode);
        assert!(matches!(
            outcome.requests.as_slice(),
            [EditorRequest::Op(MetadataOp::Load)]
        ));
        let item = EditableItem::from_json(serde_json::json!({
            "Id": "m1", "Name": "Film", "Type": "Movie", "ProductionYear": 2001,
            "Genres": ["Drama", "War"], "ProviderIds": {"Tmdb": "603"}
        }))
        .expect("item");
        editor.on_reply(Ok(MetadataReply::Loaded(Box::new(item))));
        editor
    }

    #[test]
    fn draft_round_trips_and_validates() {
        let editor = loaded_editor(EditorMode::Edit);
        assert_eq!(editor.draft.genres, "Drama, War");
        let fields = editor.draft.to_fields().expect("valid draft");
        assert_eq!(fields.genres, ["Drama", "War"]);
        assert_eq!(fields.production_year, Some(2001));

        let mut draft = editor.draft.clone();
        draft.year = "20x1".into();
        assert_eq!(draft.to_fields(), Err(ids::METADATA_ERROR_YEAR));
        draft.year.clear();
        draft.premiere_date = "2001-1-1".into();
        assert_eq!(draft.to_fields(), Err(ids::METADATA_ERROR_DATE));
        draft.premiere_date.clear();
        draft.community_rating = "11".into();
        assert_eq!(draft.to_fields(), Err(ids::METADATA_ERROR_RATING));
        draft.community_rating = "7,5".into();
        assert_eq!(draft.to_fields().map(|f| f.community_rating), Ok(Some(7.5)));
        draft.name = " ".into();
        assert_eq!(draft.to_fields(), Err(ids::METADATA_ERROR_NAME_REQUIRED));
    }

    #[test]
    fn identify_is_seeded_from_the_item_and_results_request_posters_once() {
        let mut editor = loaded_editor(EditorMode::Identify);
        assert_eq!(editor.search_name, "Film");
        assert_eq!(editor.search_year, "2001");
        assert_eq!(editor.search_ids, [("Tmdb".to_owned(), "603".to_owned())]);
        let outcome = editor.on_reply(Ok(MetadataReply::SearchResults(Vec::new())));
        assert!(outcome.requests.is_empty());
        assert_eq!(editor.results.as_deref().map(<[_]>::len), Some(0));
    }

    #[test]
    fn image_changes_reload_the_list_and_failures_show_a_message() {
        let (mut editor, outcome) =
            MetadataEditor::open("m1".into(), "Film".into(), EditorMode::Images);
        assert!(matches!(
            outcome.requests.as_slice(),
            [EditorRequest::Op(MetadataOp::ListImages)]
        ));
        let image = ItemImage {
            image_type: ImageType::Primary,
            index: 0,
            tag: Some("t".into()),
            width: None,
            height: None,
        };
        let outcome = editor.on_reply(Ok(MetadataReply::Images(vec![image.clone()])));
        assert_eq!(outcome.requests.len(), 1, "one preview request");
        let outcome = editor.on_reply(Ok(MetadataReply::Images(vec![image])));
        assert!(outcome.requests.is_empty(), "previews are requested once");
        let outcome = editor.on_reply(Ok(MetadataReply::ImagesChanged));
        assert!(matches!(
            outcome.requests.as_slice(),
            [EditorRequest::Op(MetadataOp::ListImages)]
        ));
        assert!(editor.busy);
        editor.on_reply(Err(ErrorClass::Forbidden));
        assert_eq!(editor.error, Some(ids::METADATA_ERROR_FORBIDDEN));
        assert!(!editor.busy);
    }

    #[test]
    fn saving_closes_with_a_toast() {
        let mut editor = loaded_editor(EditorMode::Edit);
        let outcome = editor.on_reply(Ok(MetadataReply::Saved));
        assert!(matches!(outcome.close, Some(Some(_))));
    }
}

//! Presentation-only cast and similar-item sections for an item's detail page.

use super::ui_text;
use kattri_i18n::ids;
use std::collections::HashMap;

use egui::{self, Color32, RichText, TextureHandle, Ui, Vec2};
use jellyfin_api::BaseItemDto;

/// Cast data needed by the UI, kept separate from networking and API ownership.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CastPerson {
    pub id: Option<String>,
    pub name: String,
    pub role: Option<String>,
    /// Jellyfin's `PrimaryImageTag`, for callers that resolve artwork.
    pub primary_image_tag: Option<String>,
}

/// The single activation produced by drawing these sections.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PeopleAction {
    pub person_id: Option<String>,
    pub item_id: Option<String>,
}

/// Draw cast and similar media. Artwork is optional and keyed by display name;
/// fetching or caching it remains the caller's responsibility.
pub fn draw(
    ui: &mut Ui,
    cast: &[CastPerson],
    similar: &[BaseItemDto],
    current_item_id: &str,
    textures_by_name: &HashMap<String, TextureHandle>,
) -> PeopleAction {
    let similar = without_current_item(current_item_id, similar);
    let mut action = PeopleAction::default();

    if section_is_visible(cast.len()) {
        ui.heading(ui_text(ids::PERSON_HEADING_CAST));
        ui.add_space(8.0);
        egui::ScrollArea::horizontal()
            .id_salt("item_detail_cast")
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    for person in cast.iter().take(CAST_LIMIT) {
                        ui.vertical(|ui| {
                            if let Some(texture) = textures_by_name.get(&person.name) {
                                ui.add(
                                    egui::Image::new(texture)
                                        .fit_to_exact_size(Vec2::new(112.0, 140.0)),
                                );
                            } else {
                                egui::Frame::new()
                                    .fill(Color32::from_rgb(31, 31, 34))
                                    .corner_radius(egui::CornerRadius::same(12))
                                    .show(ui, |ui| {
                                        ui.set_min_size(Vec2::new(112.0, 140.0));
                                        ui.centered_and_justified(|ui| {
                                            ui.label(
                                                RichText::new(initials(&person.name))
                                                    .size(26.0)
                                                    .strong()
                                                    .color(Color32::from_gray(164)),
                                            );
                                        });
                                    });
                            }
                            ui.add_space(6.0);
                            let label = match person.role.as_deref().filter(|role| !role.is_empty())
                            {
                                Some(role) => format!("{}\n{role}", person.name),
                                None => person.name.clone(),
                            };
                            let response = ui.add_enabled(
                                person.id.is_some(),
                                egui::Button::new(RichText::new(label).strong())
                                    .min_size(Vec2::new(132.0, 52.0)),
                            );
                            if response.clicked() {
                                action.person_id = person_activation_id(person);
                            }
                        });
                    }
                });
            });
    }

    if section_is_visible(similar.len()) {
        ui.add_space(16.0);
        ui.heading(ui_text(ids::DETAIL_HEADING_SIMILAR));
        ui.add_space(8.0);
        egui::ScrollArea::horizontal()
            .id_salt("item_detail_similar")
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    for item in similar {
                        ui.vertical(|ui| {
                            if let Some(texture) = textures_by_name.get(&item.name) {
                                ui.add(
                                    egui::Image::new(texture)
                                        .fit_to_exact_size(Vec2::new(156.0, 104.0)),
                                );
                            } else {
                                egui::Frame::new()
                                    .fill(Color32::from_rgb(31, 31, 34))
                                    .corner_radius(egui::CornerRadius::same(12))
                                    .show(ui, |ui| {
                                        ui.set_min_size(Vec2::new(156.0, 104.0));
                                        ui.centered_and_justified(|ui| {
                                            ui.label(
                                                RichText::new("···")
                                                    .size(24.0)
                                                    .color(Color32::from_gray(140)),
                                            );
                                        });
                                    });
                            }
                            let label = match item.item_type.as_deref() {
                                Some(kind) if !kind.is_empty() => {
                                    format!("{}\n{kind}", item.name)
                                }
                                _ => item.name.clone(),
                            };
                            if ui
                                .add(
                                    egui::Button::new(RichText::new(label).strong())
                                        .min_size(Vec2::new(156.0, 48.0)),
                                )
                                .clicked()
                            {
                                action.item_id = Some(item.id.clone());
                            }
                        });
                    }
                });
            });
    }

    action
}

const CAST_LIMIT: usize = 20;

fn initials(name: &str) -> String {
    let initials = name
        .split_whitespace()
        .filter_map(|part| part.chars().next())
        .take(2)
        .flat_map(char::to_uppercase)
        .collect::<String>();
    if initials.is_empty() {
        "•".to_owned()
    } else {
        initials
    }
}

fn section_is_visible(entry_count: usize) -> bool {
    entry_count > 0
}

fn person_activation_id(person: &CastPerson) -> Option<String> {
    person.id.clone()
}

/// Return similar items except the current item, preserving server order.
pub fn without_current_item<'a>(
    current_item_id: &str,
    items: &'a [BaseItemDto],
) -> Vec<&'a BaseItemDto> {
    items
        .iter()
        .filter(|item| item.id != current_item_id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        CAST_LIMIT, CastPerson, draw, person_activation_id, section_is_visible,
        without_current_item,
    };
    use egui::{Pos2, RawInput, Rect, Shape, Vec2};
    use jellyfin_api::BaseItemDto;

    fn item(id: &str, name: &str, item_type: Option<&str>) -> BaseItemDto {
        serde_json::from_value(serde_json::json!({
            "Id": id,
            "Name": name,
            "Type": item_type,
        }))
        .expect("minimal BaseItemDto fixture deserializes")
    }

    #[test]
    fn similar_results_exclude_current_item_and_preserve_order() {
        let items = [
            item("current", "Current", Some("Movie")),
            item("related", "Related", Some("Series")),
        ];
        let filtered = without_current_item("current", &items);

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, "related");
    }

    #[test]
    fn empty_sections_have_no_content_to_render() {
        assert!(!section_is_visible(0));
        assert!(!section_is_visible(
            without_current_item("current", &[item("current", "Current", None)]).len()
        ));
    }

    #[test]
    fn people_without_ids_cannot_produce_an_activation_id() {
        let person = CastPerson {
            id: None,
            name: "Guest performer".to_owned(),
            role: Some("Self".to_owned()),
            primary_image_tag: None,
        };
        assert_eq!(person_activation_id(&person), None);
        assert_eq!(person.name, "Guest performer");
        assert_eq!(person.role.as_deref(), Some("Self"));
    }

    #[test]
    fn cast_limit_is_twenty_and_labels_can_include_role() {
        let cast: Vec<_> = (0..CAST_LIMIT + 5)
            .map(|number| CastPerson {
                id: Some(number.to_string()),
                name: format!("Person {number}"),
                role: (number == 0).then(|| "Lead".to_owned()),
                primary_image_tag: Some(format!("tag-{number}")),
            })
            .collect();

        assert_eq!(cast.iter().take(CAST_LIMIT).count(), 20);
        assert_eq!(cast[0].role.as_deref(), Some("Lead"));
        assert!(cast[1].role.is_none());
    }

    #[test]
    fn cast_remains_visible_when_optional_similar_results_are_empty() {
        fn contains_text(output: &egui::FullOutput, expected: &str) -> bool {
            fn find(shape: &Shape, expected: &str) -> bool {
                match shape {
                    Shape::Text(text) => text.galley.job.text == expected,
                    Shape::Vec(shapes) => shapes.iter().any(|shape| find(shape, expected)),
                    _ => false,
                }
            }
            output
                .shapes
                .iter()
                .any(|clipped| find(&clipped.shape, expected))
        }

        let context = egui::Context::default();
        let mut output = context.run_ui(
            RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(640.0, 360.0))),
                ..Default::default()
            },
            |ui| {
                draw(
                    ui,
                    &[CastPerson {
                        id: Some("person-1".into()),
                        name: "Ada Vale".into(),
                        role: Some("Lead".into()),
                        primary_image_tag: None,
                    }],
                    &[],
                    "current-item",
                    &std::collections::HashMap::new(),
                );
            },
        );
        output.textures_delta.clear();

        assert!(contains_text(&output, "Cast"));
        assert!(!contains_text(&output, "More like this"));
        assert!(contains_text(&output, "Ada Vale\nLead"));
    }
}

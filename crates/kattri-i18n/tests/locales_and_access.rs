//! M4-18 and M4-19 tests: pilot message coverage, the de-DE stub and its fallback chain, and the
//! accessible-name table for interactive controls.

use std::collections::HashSet;

use kattri_i18n::{
    CONTROLS, Catalog, LocaleId, Translator, accessible_names, english, german_stub, ids,
};

fn en() -> Catalog {
    match english() {
        Ok(catalog) => catalog,
        Err(err) => panic!("en-US catalog should parse: {err}"),
    }
}

#[test]
fn every_pilot_message_id_is_unique() {
    let unique: HashSet<&str> = ids::ALL.iter().copied().collect();
    assert_eq!(unique.len(), ids::ALL.len(), "duplicate id in ids::ALL");
}

#[test]
fn every_pilot_message_id_has_a_non_empty_english_text() {
    let en = en();
    for id in ids::ALL {
        let text = en.get(id);
        assert!(
            text.is_some_and(|t| !t.trim().is_empty()),
            "{id} has no en-US text"
        );
    }
}

#[test]
fn every_control_has_a_non_empty_accessible_name_in_english() {
    let names = accessible_names(&en());
    assert_eq!(names.len(), CONTROLS.len());
    for name in &names {
        assert!(
            !name.name.trim().is_empty(),
            "control `{}` has no accessible name",
            name.control_id
        );
        assert_eq!(
            name.name,
            name.name.trim(),
            "control `{}` name has edge whitespace",
            name.control_id
        );
    }
}

#[test]
fn control_ids_are_unique_and_message_ids_exist_in_english() {
    let en = en();
    let mut control_ids = HashSet::new();
    let mut message_ids = HashSet::new();
    for control in CONTROLS {
        assert!(
            control_ids.insert(control.control_id),
            "duplicate control id `{}`",
            control.control_id
        );
        assert!(
            message_ids.insert(control.message_id),
            "two controls share message `{}`",
            control.message_id
        );
        assert!(
            control.control_id.contains('.'),
            "control ids are area.name"
        );
        assert!(
            control
                .control_id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c == '.' || c == '_'),
            "control id `{}` must be lower case",
            control.control_id
        );
        assert!(
            en.get(control.message_id).is_some(),
            "control `{}` names a message missing from en-US",
            control.control_id
        );
    }
}

#[test]
fn accessible_name_for_a_control_is_its_english_message() {
    let names = accessible_names(&en());
    let submit = names
        .iter()
        .find(|n| n.control_id == "connect.submit")
        .map(|n| n.name.as_str());
    assert_eq!(submit, Some("Connect to Jellyfin"));
}

#[test]
fn a_catalog_without_the_messages_gives_empty_names_that_the_tests_reject() {
    let empty = Catalog::new(LocaleId::en_us());
    let names = accessible_names(&empty);
    assert_eq!(names.len(), CONTROLS.len());
    assert!(names.iter().all(|n| n.name.is_empty()));
}

#[test]
fn accessible_names_are_a_pure_function_of_the_catalog() {
    let en = en();
    assert_eq!(accessible_names(&en), accessible_names(&en));
}

#[test]
fn german_stub_has_exactly_three_messages_and_all_exist_in_english() {
    let de = match german_stub() {
        Ok(catalog) => catalog,
        Err(err) => panic!("de-DE stub should parse: {err}"),
    };
    assert_eq!(de.len(), 3);
    let en = en();
    for id in de.ids() {
        assert!(en.get(id).is_some(), "stub key {id} is not in en-US");
    }
}

#[test]
fn german_region_uses_the_stub_then_falls_back_to_english() {
    let requested = match LocaleId::parse("de-DE") {
        Ok(locale) => locale,
        Err(err) => panic!("de-DE should parse: {err}"),
    };
    let catalogs = match german_stub() {
        Ok(de) => vec![en(), de],
        Err(err) => panic!("de-DE stub should parse: {err}"),
    };
    let mut translator = match Translator::new(&requested, &catalogs) {
        Ok(t) => t,
        Err(err) => panic!("translator should build: {err}"),
    };

    assert_eq!(
        translator.text(ids::CONNECT_BUTTON_SUBMIT, &[]),
        "Mit Jellyfin verbinden"
    );
    assert_eq!(
        translator.text(ids::BROWSE_BUTTON_RETRY, &[]),
        "Erneut versuchen"
    );
    // Not in the stub, so the English text is used.
    assert_eq!(
        translator.text(ids::CONNECT_FIELD_USERNAME, &[]),
        "Username"
    );
    assert_eq!(
        translator.source_locale(ids::CONNECT_FIELD_USERNAME),
        Some(&LocaleId::en_us())
    );
    assert_eq!(
        translator.source_locale(ids::CONNECT_BUTTON_SUBMIT),
        Some(&requested)
    );
    assert!(translator.missing_ids().is_empty());
}

#[test]
fn base_language_without_a_catalog_skips_straight_to_english() {
    // The stub is for de-DE only. A request for plain `de` has no `de` catalog, so the chain is
    // `de` then en-US, and the stub is not used.
    let requested = match LocaleId::parse("de") {
        Ok(locale) => locale,
        Err(err) => panic!("de should parse: {err}"),
    };
    let catalogs = match german_stub() {
        Ok(de) => vec![en(), de],
        Err(err) => panic!("de-DE stub should parse: {err}"),
    };
    let mut translator = match Translator::new(&requested, &catalogs) {
        Ok(t) => t,
        Err(err) => panic!("translator should build: {err}"),
    };
    assert_eq!(
        translator.chain_locales(),
        vec![&LocaleId::en_us()],
        "chain should hold only en-US"
    );
    assert_eq!(
        translator.text(ids::CONNECT_BUTTON_SUBMIT, &[]),
        "Connect to Jellyfin"
    );
}

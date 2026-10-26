use kattri_i18n::{
    Catalog, I18nError, LocaleId, MISSING_TEXT, TemplateError, Translator, english, ids, render,
    validate,
};

fn locale(tag: &str) -> LocaleId {
    match LocaleId::parse(tag) {
        Ok(id) => id,
        Err(err) => panic!("test locale {tag} should parse: {err}"),
    }
}

fn catalog(tag: &str, source: &str) -> Catalog {
    match Catalog::parse(locale(tag), source) {
        Ok(catalog) => catalog,
        Err(err) => panic!("test catalog {tag} should parse: {err}"),
    }
}

fn chain_strings(tag: &str) -> Vec<String> {
    locale(tag)
        .fallback_chain()
        .iter()
        .map(ToString::to_string)
        .collect()
}

// ---- locale tags and fallback order ----

#[test]
fn locale_tags_normalize_and_reject_bad_input() {
    assert_eq!(locale("de_at").to_string(), "de-AT");
    assert_eq!(locale("EN-gb").to_string(), "en-GB");
    assert_eq!(locale("fr").to_string(), "fr");
    for bad in ["", "e", "english", "de-AT-x", "de-1", "en.UTF-8", "-en"] {
        assert!(
            matches!(LocaleId::parse(bad), Err(I18nError::InvalidLocale(_))),
            "{bad:?} should be rejected"
        );
    }
}

#[test]
fn fallback_chain_order() {
    assert_eq!(chain_strings("de-AT"), ["de-AT", "de", "en-US"]);
    assert_eq!(chain_strings("fr"), ["fr", "en-US"]);
    assert_eq!(chain_strings("en-US"), ["en-US"]);
    assert_eq!(chain_strings("en-GB"), ["en-GB", "en", "en-US"]);
}

#[test]
fn lookup_prefers_requested_then_base_then_english() {
    let en = catalog("en-US", "a = English A\nb = English B\nc = English C\n");
    let de = catalog("de", "b = Base German B\n");
    let de_at = catalog("de-AT", "a = Austrian A\n");
    let mut translator = match Translator::new(&locale("de-AT"), &[en, de, de_at]) {
        Ok(t) => t,
        Err(err) => panic!("translator should build: {err}"),
    };

    assert_eq!(
        translator.text("a", &[]),
        "Austrian A",
        "requested locale wins"
    );
    assert_eq!(
        translator.text("b", &[]),
        "Base German B",
        "base language beats en-US"
    );
    assert_eq!(
        translator.text("c", &[]),
        "English C",
        "en-US is the last fallback"
    );
    assert_eq!(
        translator
            .source_locale("a")
            .map(ToString::to_string)
            .as_deref(),
        Some("de-AT")
    );
    assert_eq!(
        translator
            .source_locale("c")
            .map(ToString::to_string)
            .as_deref(),
        Some("en-US")
    );
    let order: Vec<String> = translator
        .chain_locales()
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(order, ["de-AT", "de", "en-US"]);
}

#[test]
fn unknown_locale_falls_back_to_english() {
    let en = catalog("en-US", "a = English A\n");
    let mut translator = match Translator::new(&locale("sv-SE"), &[en]) {
        Ok(t) => t,
        Err(err) => panic!("translator should build: {err}"),
    };
    assert_eq!(translator.text("a", &[]), "English A");
}

#[test]
fn en_us_catalog_is_required() {
    let de = catalog("de", "a = Hallo\n");
    assert_eq!(
        Translator::new(&locale("de"), &[de]).map(|_| ()),
        Err(I18nError::MissingFallback)
    );
}

// ---- missing keys ----

#[test]
fn missing_key_shows_marker_not_id_and_is_reported_once() {
    let en = catalog("en-US", "a = English A\n");
    let mut translator = match Translator::new(&locale("de"), &[en]) {
        Ok(t) => t,
        Err(err) => panic!("translator should build: {err}"),
    };

    let shown = translator.text("no.such.id", &[]);
    assert_eq!(shown, MISSING_TEXT);
    assert!(!shown.contains("no.such.id"));

    assert_eq!(translator.text("no.such.id", &[]), MISSING_TEXT);
    assert_eq!(translator.missing_ids(), ["no.such.id"]);
    assert_eq!(
        translator.take_new_issues(),
        ["missing message: no.such.id"]
    );
    assert!(
        translator.take_new_issues().is_empty(),
        "second drain is empty"
    );
}

#[test]
fn missing_argument_keeps_placeholder_and_reports_name_only() {
    let en = catalog("en-US", "greet = Hello {name}, you have {count} items\n");
    let mut translator = match Translator::new(&locale("en-US"), &[en]) {
        Ok(t) => t,
        Err(err) => panic!("translator should build: {err}"),
    };

    let text = translator.text("greet", &[("name", "Ada")]);
    assert_eq!(text, "Hello Ada, you have {count} items");
    let issues = translator.take_new_issues();
    assert_eq!(issues, ["missing argument `count` for message greet"]);
    assert!(
        !issues.iter().any(|issue| issue.contains("Ada")),
        "values never reach the report"
    );
}

// ---- placeholders and escaping ----

#[test]
fn placeholders_substitute_and_braces_escape() {
    let rendered = match render("Use {{literal}} and {x}", &[("x", "value")]) {
        Ok(r) => r,
        Err(err) => panic!("render should succeed: {err}"),
    };
    assert_eq!(rendered.text, "Use {literal} and value");
    assert!(rendered.missing_args.is_empty());
}

#[test]
fn substituted_values_are_not_rescanned() {
    let rendered = match render("Hi {name}", &[("name", "{other} }} {{")]) {
        Ok(r) => r,
        Err(err) => panic!("render should succeed: {err}"),
    };
    assert_eq!(rendered.text, "Hi {other} }} {{");
}

#[test]
fn malformed_templates_are_rejected_at_insert() {
    assert_eq!(validate("a {b"), Err(TemplateError::Unclosed));
    assert_eq!(validate("a } b"), Err(TemplateError::StrayBrace));
    assert_eq!(validate("{}"), Err(TemplateError::BadPlaceholder));
    assert_eq!(validate("{a-b}"), Err(TemplateError::BadPlaceholder));
    assert_eq!(validate("{{ok}} {x_1}"), Ok(()));

    let mut cat = Catalog::new(LocaleId::en_us());
    assert!(matches!(
        cat.insert("bad", "a {b"),
        Err(I18nError::Template { ref id, reason: TemplateError::Unclosed }) if id == "bad"
    ));
    assert!(cat.is_empty());
}

#[test]
fn escaped_braces_in_catalog_file_render_literally() {
    let cat = catalog("en-US", "brace = Use {{x}} for {v}\n");
    let mut translator = match Translator::new(&locale("en-US"), &[cat]) {
        Ok(t) => t,
        Err(err) => panic!("translator should build: {err}"),
    };
    assert_eq!(translator.text("brace", &[("v", "y")]), "Use {x} for y");
}

// ---- file format and round trip ----

#[test]
fn round_trip_preserves_catalog_with_escapes() {
    let mut cat = Catalog::new(LocaleId::en_us());
    if let Err(err) = cat.insert("multi.line", "first\nsecond\ttab \\ back {{x}} {y}") {
        panic!("insert should succeed: {err}");
    }
    if let Err(err) = cat.insert("eq", "a = b") {
        panic!("insert should succeed: {err}");
    }
    let source = cat.to_source();
    assert!(
        source.contains("\\n") && source.contains("\\\\"),
        "escapes are written: {source}"
    );
    let reparsed = match Catalog::parse(LocaleId::en_us(), &source) {
        Ok(c) => c,
        Err(err) => panic!("round trip should parse: {err}"),
    };
    assert_eq!(reparsed, cat);
    assert_eq!(reparsed.to_source(), source, "serialization is stable");
}

#[test]
fn shipped_english_catalog_round_trips() {
    let en = match english() {
        Ok(c) => c,
        Err(err) => panic!("shipped catalog should parse: {err}"),
    };
    let reparsed = match Catalog::parse(LocaleId::en_us(), &en.to_source()) {
        Ok(c) => c,
        Err(err) => panic!("round trip should parse: {err}"),
    };
    assert_eq!(reparsed, en);
}

#[test]
fn comments_blank_lines_and_value_equals_sign() {
    let cat = catalog(
        "en-US",
        "# comment\n\n   \nkey.one =   spaced  \nkey.two = x=y\n",
    );
    assert_eq!(cat.get("key.one"), Some("spaced"));
    assert_eq!(cat.get("key.two"), Some("x=y"));
    assert_eq!(cat.len(), 2);
}

#[test]
fn parse_errors_name_the_line_and_never_echo_text() {
    let cases: [(&str, I18nError); 4] = [
        ("novalue\n", I18nError::MissingSeparator { line: 1 }),
        (
            "ok = fine\nbad key = x\n",
            I18nError::InvalidKey { line: 2 },
        ),
        ("a = bad \\q escape\n", I18nError::BadEscape { line: 1 }),
        ("a = 1\na = 2\n", I18nError::DuplicateId("a".to_owned())),
    ];
    for (source, expected) in cases {
        assert_eq!(
            Catalog::parse(LocaleId::en_us(), source),
            Err(expected),
            "source {source:?}"
        );
    }

    let mut cat = Catalog::new(LocaleId::en_us());
    assert_eq!(
        cat.insert("pad", " spaced "),
        Err(I18nError::WhitespaceEdge("pad".to_owned()))
    );
    assert_eq!(
        cat.insert("", "x"),
        Err(I18nError::InvalidId(String::new()))
    );
}

// ---- shipped English catalog and message ids ----

#[test]
fn every_pilot_message_id_exists_in_en_us() {
    let en = match english() {
        Ok(c) => c,
        Err(err) => panic!("shipped catalog should parse: {err}"),
    };
    for id in ids::ALL {
        assert!(en.get(id).is_some(), "message id {id} missing from en-US");
    }
    assert_eq!(
        en.len(),
        ids::ALL.len(),
        "catalog holds exactly the pilot ids"
    );
}

#[test]
fn english_placeholder_renders_with_value() {
    let en = match english() {
        Ok(c) => c,
        Err(err) => panic!("shipped catalog should parse: {err}"),
    };
    let mut translator = match Translator::new(&LocaleId::en_us(), &[en]) {
        Ok(t) => t,
        Err(err) => panic!("translator should build: {err}"),
    };
    assert_eq!(
        translator.text(ids::BROWSE_FILTER_UNSUPPORTED, &[("filter", "Played")]),
        "Jellyfin does not support the Played filter for this library. It has been hidden."
    );
    assert_eq!(
        translator.text(ids::CONNECT_CHECKING_SERVER, &[]),
        "Checking server…"
    );
    assert!(translator.take_new_issues().is_empty());
}

#[test]
fn second_locale_file_changes_label_text() {
    let de = catalog(
        "de-AT",
        "connect.status.checking_server = Server wird geprüft…\n",
    );
    let en = match english() {
        Ok(c) => c,
        Err(err) => panic!("shipped catalog should parse: {err}"),
    };
    let mut in_german = match Translator::new(&locale("de-AT"), &[en.clone(), de]) {
        Ok(t) => t,
        Err(err) => panic!("translator should build: {err}"),
    };
    let mut in_english = match Translator::new(&locale("en-US"), &[en]) {
        Ok(t) => t,
        Err(err) => panic!("translator should build: {err}"),
    };
    assert_eq!(
        in_german.text(ids::CONNECT_CHECKING_SERVER, &[]),
        "Server wird geprüft…"
    );
    assert_eq!(
        in_english.text(ids::CONNECT_CHECKING_SERVER, &[]),
        "Checking server…"
    );
    assert_eq!(
        in_german.text(ids::BROWSE_SORT_DATE_ADDED, &[]),
        "Date added",
        "untranslated ids fall back to en-US"
    );
}

use kattri_parental::{RATING_LADDER, RatingDecision, check_rating_text, level_for};

#[test]
fn every_ladder_value_maps_to_its_position() {
    let expected = [
        ("TV-Y", 0),
        ("TV-Y7", 1),
        ("G", 2),
        ("TV-G", 3),
        ("PG", 4),
        ("TV-PG", 5),
        ("PG-13", 6),
        ("TV-14", 7),
        ("R", 8),
        ("TV-MA", 9),
        ("NC-17", 10),
    ];
    assert_eq!(RATING_LADDER.len(), expected.len());
    for (text, level) in expected {
        assert_eq!(level_for(text), Some(level), "{text}");
    }
}

#[test]
fn matching_ignores_ascii_case_and_surrounding_whitespace() {
    assert_eq!(level_for("pg-13"), Some(6));
    assert_eq!(level_for("  TV-MA\t"), Some(9));
    assert_eq!(level_for(" tv-y7 "), Some(1));
}

#[test]
fn blank_and_unknown_text_has_no_level() {
    for text in ["", "   ", "PG-16", "TV-PG-13", "Unrated", "R18", "PG 13"] {
        assert_eq!(level_for(text), None, "{text:?}");
    }
}

#[test]
fn no_ceiling_allows_any_rating_text() {
    assert_eq!(
        check_rating_text(None, Some("NC-17")),
        RatingDecision::Allow
    );
    assert_eq!(
        check_rating_text(None, Some("mystery")),
        RatingDecision::Allow
    );
    assert_eq!(check_rating_text(None, None), RatingDecision::Allow);
}

#[test]
fn items_at_or_below_the_ceiling_are_allowed() {
    assert_eq!(
        check_rating_text(Some(6), Some("PG-13")),
        RatingDecision::Allow
    );
    assert_eq!(check_rating_text(Some(6), Some("g")), RatingDecision::Allow);
    assert_eq!(
        check_rating_text(Some(10), Some("NC-17")),
        RatingDecision::Allow
    );
}

#[test]
fn items_above_the_ceiling_are_blocked() {
    assert_eq!(check_rating_text(Some(6), Some("R")), RatingDecision::Block);
    assert_eq!(
        check_rating_text(Some(0), Some("TV-Y7")),
        RatingDecision::Block
    );
}

#[test]
fn unknown_and_blank_text_is_blocked_under_a_ceiling() {
    assert_eq!(
        check_rating_text(Some(10), Some("Unrated")),
        RatingDecision::Block
    );
    assert_eq!(check_rating_text(Some(10), Some("")), RatingDecision::Block);
    assert_eq!(
        check_rating_text(Some(10), Some("   ")),
        RatingDecision::Block
    );
}

#[test]
fn a_missing_rating_is_blocked_under_a_ceiling() {
    assert_eq!(check_rating_text(Some(0), None), RatingDecision::Block);
}

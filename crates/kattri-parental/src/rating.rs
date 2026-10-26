/// Outcome of comparing an item's rating with the profile's ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RatingDecision {
    /// Rated at or below the ceiling, or no ceiling is set.
    Allow,
    /// The server returned no rating, so the item is shown. The UI must say the rating is unknown.
    AllowUnrated,
    /// Rated above the ceiling.
    Block,
}

/// Compares a numeric content rating with a numeric ceiling. Both come from the server's
/// parental rating fields. This function does not interpret rating names.
pub fn check_rating(ceiling: Option<u32>, content_rating: Option<u32>) -> RatingDecision {
    match (ceiling, content_rating) {
        (None, _) => RatingDecision::Allow,
        (Some(_), None) => RatingDecision::AllowUnrated,
        (Some(max), Some(rating)) if rating <= max => RatingDecision::Allow,
        (Some(_), Some(_)) => RatingDecision::Block,
    }
}

/// The owner-approved rating ladder, lowest first (owner answer 2026-10-22). Movie and TV
/// ratings share one ladder. A stored ceiling is an index into this list, so the settings file
/// keeps the level, not the text.
pub const RATING_LADDER: [&str; 11] = [
    "TV-Y", "TV-Y7", "G", "TV-G", "PG", "TV-PG", "PG-13", "TV-14", "R", "TV-MA", "NC-17",
];

/// The ladder level of a rating text. Surrounding whitespace is ignored and ASCII case does
/// not matter. Blank or unknown text has no level, so it is not placed on the ladder.
pub fn level_for(text: &str) -> Option<u8> {
    let text = text.trim();
    RATING_LADDER
        .iter()
        .position(|name| name.eq_ignore_ascii_case(text))
        .and_then(|index| u8::try_from(index).ok())
}

/// Compares a server rating text with a ladder ceiling. A missing rating (no text at all) is
/// allowed and reported as unrated. Blank or unknown text is blocked while a ceiling is set,
/// because the app cannot tell whether it is above the ceiling.
pub fn check_rating_text(ceiling: Option<u32>, official_rating: Option<&str>) -> RatingDecision {
    match (ceiling, official_rating) {
        (None, _) => RatingDecision::Allow,
        (Some(_), None) => RatingDecision::AllowUnrated,
        (Some(max), Some(text)) => match level_for(text) {
            Some(level) if u32::from(level) <= max => RatingDecision::Allow,
            _ => RatingDecision::Block,
        },
    }
}

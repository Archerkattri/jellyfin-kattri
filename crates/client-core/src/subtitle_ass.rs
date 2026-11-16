//! Typed policy for the mpv `sub-ass-override` option (M6-10, with M6-11 for the user style).
//!
//! Plain-text subtitle tracks and authored ASS/SSA scripts are rendered on different paths. The
//! client writes its user style options (`sub-font`, `sub-scale`, `sub-color`, ...) when a file
//! opens, so the override mode for ASS/SSA has to be explicit: leaving the option unset lets the
//! player's own default decide, and a stored value that the player does not recognise must never
//! reach it verbatim.
//!
//! What is known, and from where:
//! - The captured pinned libmpv runtime inventory
//!   (`crates/client-core/tests/fixtures/mpv-runtime-options.json`, mpv v0.41.0-1286-gfa0534a72)
//!   contains `sub-ass`, `sub-ass-override` and `sub-ass-style-overrides`. The allowlist in
//!   [`crate::mpv_options`] already spells the five documented override values.
//! - The pinned player source at the commit the fixture names (`sub/sd_ass.c` and
//!   `options/options.c` at `fa0534a729`) applies the override only to subtitles that are not
//!   converted from a plain-text format: converted tracks always receive the user style
//!   (`converted || ...` in `configure_ass`), and `no` leaves an authored ASS/SSA script's own
//!   styles, font scale and style-override list untouched. The player's own default is `scale`,
//!   not `no`.
//!
//! What is unknown or unvalidated:
//! - No ASS/SSA fixture was rendered and no display was available, so the exact rendering effect
//!   of each value on the pinned build is not measured here. The pinned source is documentation
//!   evidence, not a rendering test.
//! - The secondary subtitle path uses its own option (`secondary-sub-ass-override`) and is not
//!   covered by this policy.
//!
//! The product default for an authored ASS/SSA track is therefore
//! [`AssStyleOverride::DEFAULT_FOR_ASS_AND_SSA`]: keep what the script authored. A stored explicit
//! choice wins, including `force`. A stored value that is not one of the five spellings is
//! refused and the default is used, so it is never passed to the player.

use std::fmt;

use crate::mpv_options::{MpvOptionError, OptionValue, ValidatedOption, validate};

/// The `sub-ass-override` value that keeps an authored ASS/SSA script's own styles.
pub const ASS_STYLE_OVERRIDE_NAME: &str = "sub-ass-override";

/// The `sub-ass-style-overrides` value shape the load path writes.
pub const ASS_STYLE_OVERRIDES_NAME: &str = "sub-ass-style-overrides";

/// The mpv `sub-ass-override` modes. The spellings are the player's own, not this client's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssStyleOverride {
    /// `no`: keep the authored ASS/SSA styles. The product default for such tracks.
    KeepAuthored,
    /// `yes`: apply the user style overrides the player considers set.
    UserStyle,
    /// `force`: apply the full user style over the authored styles.
    ForceUserStyle,
    /// `scale`: apply only the user subtitle scale.
    ScaleOnly,
    /// `strip`: strip ASS tags and render the text with the user style.
    StripTags,
}

impl AssStyleOverride {
    /// The default for an authored ASS/SSA track: play it as authored until the user opts in.
    pub const DEFAULT_FOR_ASS_AND_SSA: Self = Self::KeepAuthored;

    /// Every mode, in display order.
    pub const ALL: [Self; 5] = [
        Self::KeepAuthored,
        Self::UserStyle,
        Self::ForceUserStyle,
        Self::ScaleOnly,
        Self::StripTags,
    ];

    /// The exact value mpv takes.
    pub const fn mpv_value(self) -> &'static str {
        match self {
            Self::KeepAuthored => "no",
            Self::UserStyle => "yes",
            Self::ForceUserStyle => "force",
            Self::ScaleOnly => "scale",
            Self::StripTags => "strip",
        }
    }

    /// Parse one stored value. The five player spellings are accepted after trimming; anything
    /// else is refused, so a stale or hand-edited setting never reaches the player.
    pub fn parse(raw: &str) -> Result<Self, AssStyleOverrideError> {
        match raw.trim() {
            "no" => Ok(Self::KeepAuthored),
            "yes" => Ok(Self::UserStyle),
            "force" => Ok(Self::ForceUserStyle),
            "scale" => Ok(Self::ScaleOnly),
            "strip" => Ok(Self::StripTags),
            _ => Err(AssStyleOverrideError::UnknownSpelling),
        }
    }

    /// Parse an optional stored value. An absent or blank setting means "no explicit choice", so
    /// the caller uses [`AssStyleOverride::DEFAULT_FOR_ASS_AND_SSA`].
    pub fn from_stored(stored: Option<&str>) -> Result<Option<Self>, AssStyleOverrideError> {
        match stored.map(str::trim).filter(|value| !value.is_empty()) {
            Some(value) => Self::parse(value).map(Some),
            None => Ok(None),
        }
    }

    /// Resolve the mode to write, and say whether a stored value was refused.
    pub fn resolve_stored(stored: Option<&str>) -> ResolvedAssOverride {
        match Self::from_stored(stored) {
            Ok(Some(mode)) => ResolvedAssOverride {
                mode,
                refusal: None,
            },
            Ok(None) => ResolvedAssOverride {
                mode: Self::DEFAULT_FOR_ASS_AND_SSA,
                refusal: None,
            },
            Err(error) => ResolvedAssOverride {
                mode: Self::DEFAULT_FOR_ASS_AND_SSA,
                refusal: Some(error),
            },
        }
    }
}

/// The mode to write for the current settings, plus why a stored value was ignored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResolvedAssOverride {
    /// The mode to write. An explicit stored mode always wins; otherwise the ASS/SSA default.
    pub mode: AssStyleOverride,
    /// Set when the stored text was refused and `mode` is the default instead.
    pub refusal: Option<AssStyleOverrideError>,
}

/// Why a stored override value was refused. The variant does not carry the refused text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssStyleOverrideError {
    /// The text is not one of the five documented spellings.
    UnknownSpelling,
}

impl fmt::Display for AssStyleOverrideError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownSpelling => {
                f.write_str("unknown ASS style override; expected no, yes, force, scale, or strip")
            }
        }
    }
}

impl std::error::Error for AssStyleOverrideError {}

/// The checked load-path writes for ASS and SSA styling.
///
/// The scale-border choice is always written, as before. The override is always written too: an
/// unset setting writes [`AssStyleOverride::DEFAULT_FOR_ASS_AND_SSA`], so a file no longer depends
/// on the player's own default, and an explicit stored choice wins. The values are checked
/// against the [`crate::mpv_options`] allowlist before any caller can send them.
pub fn subtitle_ass_options(
    scale_border_and_shadow: bool,
    mode: AssStyleOverride,
) -> Result<Vec<ValidatedOption>, MpvOptionError> {
    let scaled = if scale_border_and_shadow {
        "ScaledBorderAndShadow=yes"
    } else {
        "ScaledBorderAndShadow=no"
    };
    Ok(vec![
        validate(ASS_STYLE_OVERRIDES_NAME, OptionValue::Text(scaled))?,
        validate(ASS_STYLE_OVERRIDE_NAME, OptionValue::Text(mode.mpv_value()))?,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mpv_options::{ALLOWLIST, ValueKind};

    #[test]
    fn every_mode_spelling_round_trips() {
        for mode in AssStyleOverride::ALL {
            assert_eq!(AssStyleOverride::parse(mode.mpv_value()), Ok(mode));
        }
    }

    #[test]
    fn the_modes_are_exactly_the_player_spellings() {
        let spellings: Vec<&str> = AssStyleOverride::ALL
            .iter()
            .map(|mode| mode.mpv_value())
            .collect();
        assert_eq!(spellings, ["no", "yes", "force", "scale", "strip"]);
    }

    #[test]
    fn every_mode_passes_the_allowlist_entry() {
        let spec = ALLOWLIST
            .iter()
            .find(|spec| spec.name == ASS_STYLE_OVERRIDE_NAME)
            .expect("the override is on the allowlist");
        let ValueKind::Enum(allowed) = spec.kind else {
            panic!("the override entry must be an enum");
        };
        for mode in AssStyleOverride::ALL {
            assert!(
                allowed.contains(&mode.mpv_value()),
                "{mode:?} is not in the allowlist enum"
            );
            assert!(
                validate(ASS_STYLE_OVERRIDE_NAME, OptionValue::Text(mode.mpv_value())).is_ok(),
                "{mode:?}"
            );
        }
    }

    #[test]
    fn unknown_or_blank_stored_values_are_handled_explicitly() {
        for bad in ["", "  ", "banana", "FORCE", "force ", "fo rce"] {
            // `force ` trims to a known value; every other entry is refused.
            let parsed = AssStyleOverride::from_stored(Some(bad));
            if bad.trim() == "force" {
                assert!(parsed.is_ok());
            } else if bad.trim().is_empty() {
                assert_eq!(parsed, Ok(None));
            } else {
                assert!(
                    parsed.is_err(),
                    "{bad:?} must not become a mode: {parsed:?}"
                );
            }
        }
        for bad in ["banana", "FORCE", "fo rce"] {
            assert_eq!(
                AssStyleOverride::parse(bad),
                Err(AssStyleOverrideError::UnknownSpelling)
            );
        }
    }

    #[test]
    fn the_ass_default_keeps_authored_styles_and_survives_an_absent_setting() {
        assert_eq!(
            AssStyleOverride::DEFAULT_FOR_ASS_AND_SSA,
            AssStyleOverride::KeepAuthored
        );
        assert_eq!(AssStyleOverride::DEFAULT_FOR_ASS_AND_SSA.mpv_value(), "no");
        for stored in [None, Some(""), Some("   ")] {
            let resolved = AssStyleOverride::resolve_stored(stored);
            assert_eq!(resolved.mode, AssStyleOverride::KeepAuthored, "{stored:?}");
            assert_eq!(resolved.refusal, None, "{stored:?}");
        }
    }

    #[test]
    fn an_explicit_stored_override_always_wins() {
        for mode in AssStyleOverride::ALL {
            let resolved = AssStyleOverride::resolve_stored(Some(mode.mpv_value()));
            assert_eq!(resolved.mode, mode, "{mode:?}");
            assert_eq!(resolved.refusal, None, "{mode:?}");
        }
        // The explicit override still wins over the ASS/SSA default, including force.
        assert_eq!(
            AssStyleOverride::resolve_stored(Some("force")).mode,
            AssStyleOverride::ForceUserStyle
        );
    }

    #[test]
    fn a_refused_stored_value_falls_back_to_the_default_and_is_reported() {
        let resolved = AssStyleOverride::resolve_stored(Some("banana"));
        assert_eq!(resolved.mode, AssStyleOverride::KeepAuthored);
        assert_eq!(
            resolved.refusal,
            Some(AssStyleOverrideError::UnknownSpelling)
        );
    }

    #[test]
    fn the_load_path_writes_the_scale_choice_and_the_resolved_override() {
        for scale_border_and_shadow in [true, false] {
            let options = subtitle_ass_options(
                scale_border_and_shadow,
                AssStyleOverride::DEFAULT_FOR_ASS_AND_SSA,
            )
            .expect("the default write is allowlisted");
            assert_eq!(options.len(), 2);
            assert_eq!(options[0].name, ASS_STYLE_OVERRIDES_NAME);
            assert_eq!(
                options[0].value,
                if scale_border_and_shadow {
                    "ScaledBorderAndShadow=yes"
                } else {
                    "ScaledBorderAndShadow=no"
                }
            );
            assert_eq!(options[1].name, ASS_STYLE_OVERRIDE_NAME);
            assert_eq!(options[1].value, "no");
        }
    }

    #[test]
    fn the_load_path_writes_an_explicit_mode_verbatim() {
        for mode in AssStyleOverride::ALL {
            let options = subtitle_ass_options(true, mode).expect("mode is allowlisted");
            assert_eq!(options[1].name, ASS_STYLE_OVERRIDE_NAME);
            assert_eq!(options[1].value, mode.mpv_value());
        }
    }
}

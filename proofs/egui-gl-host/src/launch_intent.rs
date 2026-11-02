use std::{
    ffi::{OsStr, OsString},
    fmt,
};

use kattri_client_core::deep_link::DeepLinkIntent;

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct HostArgs {
    pub(super) profile: Option<String>,
    pub(super) media: Vec<OsString>,
    pub(super) launch_intent: Option<DeepLinkIntent>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HostArgsError {
    MissingProfileName,
    InvalidProfileName,
    DeepLinkUsedAsProfile,
    UnknownOption,
    InvalidDeepLink,
    MultipleDeepLinks,
    ConflictingLaunchIntents,
}

impl fmt::Display for HostArgsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::MissingProfileName => "--profile needs a profile name",
            Self::InvalidProfileName => "profile name must be valid UTF-8",
            Self::DeepLinkUsedAsProfile => "a Jellyfin link cannot be used as a profile name",
            Self::UnknownOption => "unknown command-line option",
            Self::InvalidDeepLink => "Jellyfin link is invalid",
            Self::MultipleDeepLinks => "only one Jellyfin link may be supplied",
            Self::ConflictingLaunchIntents => {
                "a Jellyfin link cannot be combined with a local media path"
            }
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for HostArgsError {}

pub(super) fn parse_host_args<I>(args: I) -> Result<HostArgs, HostArgsError>
where
    I: IntoIterator<Item = OsString>,
{
    let mut parsed = HostArgs::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == OsStr::new("--profile") {
            let name = args.next().ok_or(HostArgsError::MissingProfileName)?;
            if is_jellyfin_scheme_candidate(&name) {
                return Err(HostArgsError::DeepLinkUsedAsProfile);
            }
            parsed.profile = Some(
                name.into_string()
                    .map_err(|_| HostArgsError::InvalidProfileName)?,
            );
        } else if let Some(name) = arg
            .to_str()
            .and_then(|text| text.strip_prefix("--profile="))
        {
            if is_jellyfin_scheme_candidate(OsStr::new(name)) {
                return Err(HostArgsError::DeepLinkUsedAsProfile);
            }
            parsed.profile = Some(name.to_owned());
        } else if is_jellyfin_scheme_candidate(&arg) {
            let text = arg.to_str().ok_or(HostArgsError::InvalidDeepLink)?;
            let intent = DeepLinkIntent::parse(text).map_err(|_| HostArgsError::InvalidDeepLink)?;
            if parsed.launch_intent.replace(intent).is_some() {
                return Err(HostArgsError::MultipleDeepLinks);
            }
        } else if arg.to_str().is_some_and(|text| text.starts_with("--")) {
            return Err(HostArgsError::UnknownOption);
        } else {
            parsed.media.push(arg);
        }
    }

    if parsed.launch_intent.is_some() && !parsed.media.is_empty() {
        return Err(HostArgsError::ConflictingLaunchIntents);
    }
    Ok(parsed)
}

fn is_jellyfin_scheme_candidate(value: &OsStr) -> bool {
    value
        .to_str()
        .map(|text| text.trim_start_matches(['"', '\'']))
        .and_then(|text| text.get(.."jellyfin:".len()))
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("jellyfin:"))
}

#[cfg(test)]
mod tests {
    use super::{HostArgs, HostArgsError, parse_host_args};
    use std::ffi::OsString;

    const VALID_LINK: &str = "jellyfin://kattri/v1/item/0123456789abcdef0123456789abcdef?server=https%3A%2F%2Fmedia.example%2Fjellyfin";

    fn parse(args: &[&str]) -> Result<HostArgs, HostArgsError> {
        parse_host_args(args.iter().map(|arg| OsString::from(*arg)))
    }

    #[test]
    fn no_arguments_selects_no_profile_media_or_link() {
        assert_eq!(parse(&[]), Ok(HostArgs::default()));
    }

    #[test]
    fn profile_forms_and_local_media_are_preserved() {
        assert_eq!(
            parse(&["--profile", "kids", "clip.mkv"]),
            Ok(HostArgs {
                profile: Some("kids".to_owned()),
                media: vec![OsString::from("clip.mkv")],
                launch_intent: None,
            })
        );
        assert_eq!(
            parse(&["--profile=kids"]).map(|args| args.profile),
            Ok(Some("kids".to_owned()))
        );
    }

    #[test]
    fn a_deep_link_is_parsed_and_can_keep_the_selected_profile() {
        let args = parse(&["--profile", "kids", VALID_LINK]).expect("valid launch args");
        let intent = args.launch_intent.expect("link intent");
        assert_eq!(args.profile.as_deref(), Some("kids"));
        assert!(args.media.is_empty());
        assert_eq!(intent.item_id(), "0123456789abcdef0123456789abcdef");
        assert_eq!(intent.server_url(), "https://media.example/jellyfin");
    }

    #[test]
    fn deep_links_cannot_be_combined_with_local_media_or_each_other() {
        assert_eq!(
            parse(&["clip.mkv", VALID_LINK]),
            Err(HostArgsError::ConflictingLaunchIntents)
        );
        assert_eq!(
            parse(&[VALID_LINK, VALID_LINK]),
            Err(HostArgsError::MultipleDeepLinks)
        );
    }

    #[test]
    fn play_actions_are_not_launch_intents() {
        let play_route = VALID_LINK.replace("/item/", "/play/");
        let play_query = format!("{VALID_LINK}&play=true");
        assert_eq!(parse(&[&play_route]), Err(HostArgsError::InvalidDeepLink));
        assert_eq!(parse(&[&play_query]), Err(HostArgsError::InvalidDeepLink));
    }

    #[test]
    fn quoted_or_encoded_argument_injection_is_rejected_without_echoing_input() {
        let quoted = format!("\"{VALID_LINK}\"");
        assert_eq!(parse(&[&quoted]), Err(HostArgsError::InvalidDeepLink));

        let encoded_injection = format!("{VALID_LINK}%22%20--profile%3Dkids");
        let error = parse(&[&encoded_injection]).expect_err("unsafe link tail");
        assert_eq!(error, HostArgsError::InvalidDeepLink);
        assert!(!error.to_string().contains("media.example"));
    }

    #[test]
    fn malformed_links_and_ambiguous_options_have_sanitized_errors() {
        let malformed = "jellyfin://kattri/v1/item/not-a-guid?server=https%3A%2F%2Fmedia.example";
        let error = parse(&[malformed]).expect_err("invalid item id");
        assert_eq!(error, HostArgsError::InvalidDeepLink);
        assert!(!error.to_string().contains("media.example"));
        assert_eq!(
            parse(&["--profile", "jellyfin://kattri/v1/item/id"]),
            Err(HostArgsError::DeepLinkUsedAsProfile)
        );
        let inline_link_profile = format!("--profile={VALID_LINK}");
        assert_eq!(
            parse(&[&inline_link_profile]),
            Err(HostArgsError::DeepLinkUsedAsProfile)
        );
        assert_eq!(
            parse(&["--profile"]),
            Err(HostArgsError::MissingProfileName)
        );
        assert_eq!(parse(&["--profle"]), Err(HostArgsError::UnknownOption));
    }
}

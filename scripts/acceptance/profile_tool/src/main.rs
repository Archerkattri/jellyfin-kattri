use std::{
    env,
    error::Error,
    path::{Path, PathBuf},
    process::ExitCode,
};

use kattri_settings::{ProfileStore, validate_profile_name};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("acceptance profile setup refused: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut arguments = env::args_os().skip(1);
    let command = arguments
        .next()
        .and_then(|value| value.into_string().ok())
        .ok_or("command is required")?;
    let profiles_dir = arguments
        .next()
        .map(PathBuf::from)
        .ok_or("profiles directory is required")?;
    let profile_name = arguments
        .next()
        .and_then(|value| value.into_string().ok())
        .ok_or("profile name must be UTF-8")?;
    let expected_active = arguments.next().and_then(|value| value.into_string().ok());
    if arguments.next().is_some() {
        return Err("unexpected extra argument".into());
    }

    if command == "initialize-macos-runner" {
        if expected_active.is_some() {
            return Err("runner initialization does not accept an active-profile argument".into());
        }
        return initialize_macos_runner(&profiles_dir, &profile_name);
    }

    let index_path = profiles_dir.join("index.toml");
    if !index_path.is_file() {
        return Err(
            "an existing profile index is required; refusing first-run initialization".into(),
        );
    }
    let mut store = ProfileStore::open(&profiles_dir)?;
    let original_default = store.default_profile().to_owned();
    let original_active = store.active_profile().to_owned();
    match command.as_str() {
        "create" => {
            validate_acceptance_name(&profile_name)?;
            if store.names().iter().any(|name| name == &profile_name)
                || profiles_dir.join(&profile_name).exists()
            {
                return Err("the requested acceptance profile already exists".into());
            }
            store.create(&profile_name)?;
        }
        "verify" => {
            validate_acceptance_name(&profile_name)?;
            require_listed_profile(&store, &profile_name)?;
            if let Some(expected) = expected_active.as_deref()
                && store.active_profile() != expected
            {
                return Err("active profile does not match the expected selection".into());
            }
        }
        _ => return Err("command must be create or verify".into()),
    }
    if store.default_profile() != original_default {
        return Err("operation changed the default profile pointer".into());
    }
    println!(
        "{command} {profile_name}; default={original_default}; prior_active={original_active}; active_now={}; profile_count={}",
        store.active_profile(),
        store.names().len(),
    );
    Ok(())
}

fn validate_acceptance_name(name: &str) -> Result<(), Box<dyn Error>> {
    if !name.starts_with("acceptance-") {
        return Err("profile name must start with acceptance-".into());
    }
    validate_profile_name(name)?;
    Ok(())
}

fn require_listed_profile(store: &ProfileStore, name: &str) -> Result<(), Box<dyn Error>> {
    if !store.names().iter().any(|profile| profile == name) {
        return Err("the requested profile is not listed".into());
    }
    if !store.settings_path(name)?.is_file() {
        return Err("the requested profile settings file is missing".into());
    }
    Ok(())
}

fn initialize_macos_runner(profiles_dir: &Path, profile_name: &str) -> Result<(), Box<dyn Error>> {
    if !env::var("GITHUB_ACTIONS").is_ok_and(|value| value == "true")
        || !env::var("RUNNER_OS").is_ok_and(|value| value == "macOS")
        || !env::var_os("RUNNER_TEMP").is_some_and(|path| Path::new(&path).is_dir())
    {
        return Err(
            "first-run initialization is restricted to an ephemeral GitHub macOS runner".into(),
        );
    }
    let home = env::var_os("HOME").ok_or("HOME is unavailable on the GitHub runner")?;
    let expected_profiles_dir = PathBuf::from(home)
        .join("Library")
        .join("Application Support")
        .join("org.Jellyfin.Kattri")
        .join("profiles");
    if profiles_dir.to_path_buf() != expected_profiles_dir {
        return Err(
            "runner profile path does not match Kattri's macOS ProjectDirs location".into(),
        );
    }
    if profiles_dir.exists() {
        return Err(
            "runner profile directory already exists; refusing to initialize unknown state".into(),
        );
    }
    validate_acceptance_name(profile_name)?;

    let mut store = ProfileStore::open(profiles_dir)?;
    if store.default_profile() != "default" || store.active_profile() != "default" {
        return Err("new runner profile store did not start with default pointers".into());
    }
    store.create(profile_name)?;
    if store.default_profile() != "default"
        || store.active_profile() != "default"
        || !store.names().iter().any(|name| name == profile_name)
    {
        return Err("acceptance profile creation changed the runner's default selection".into());
    }
    println!(
        "initialized macOS runner profile store; default=default; active=default; added={profile_name}"
    );
    Ok(())
}

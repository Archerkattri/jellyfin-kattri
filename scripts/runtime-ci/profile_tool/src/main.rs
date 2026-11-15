use std::{env, error::Error, path::PathBuf, process::ExitCode};

use directories::ProjectDirs;
use kattri_settings::{ProfileStore, validate_profile_name};

const PROFILE_NAME: &str = "acceptance-linux-ci";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("runtime acceptance profile refused: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut arguments = env::args().skip(1);
    let command = arguments.next().ok_or("command is required")?;
    let profile_name = arguments.next().ok_or("profile name is required")?;
    if arguments.next().is_some() {
        return Err("unexpected extra argument".into());
    }
    require_ephemeral_linux_runner()?;
    validate_profile_name(&profile_name)?;
    if profile_name != PROFILE_NAME || !profile_name.starts_with("acceptance-") {
        return Err("profile name is outside this workflow's fixed acceptance profile".into());
    }

    let profiles_dir = profile_directory()?;
    match command.as_str() {
        "initialize-linux-runner" => {
            if profiles_dir.exists() {
                return Err(
                    "runner profile directory already exists; refusing unknown state".into(),
                );
            }
            let mut store = ProfileStore::open(&profiles_dir)?;
            require_default_pointers(&store)?;
            store.create(&profile_name)?;
            require_default_pointers(&store)?;
            require_listed_profile(&store, &profile_name)?;
            println!(
                "initialized Linux runner profile store; default=default; active=default; added={profile_name}"
            );
        }
        "verify-linux-runner" => {
            if !profiles_dir.is_dir() {
                return Err("runner profile directory is missing".into());
            }
            let store = ProfileStore::open(&profiles_dir)?;
            require_default_pointers(&store)?;
            require_listed_profile(&store, &profile_name)?;
            println!(
                "verified Linux runner profile; default=default; active=default; selected={profile_name}"
            );
        }
        _ => return Err("command must be initialize-linux-runner or verify-linux-runner".into()),
    }
    Ok(())
}

fn require_ephemeral_linux_runner() -> Result<(), Box<dyn Error>> {
    if !env::var("GITHUB_ACTIONS").is_ok_and(|value| value == "true")
        || !env::var("RUNNER_OS").is_ok_and(|value| value == "Linux")
    {
        return Err("profile setup is restricted to a GitHub-hosted Linux runner".into());
    }
    let runner_temp = env::var_os("RUNNER_TEMP").ok_or("RUNNER_TEMP is unavailable")?;
    if !PathBuf::from(&runner_temp).is_dir() {
        return Err("RUNNER_TEMP is not an existing directory".into());
    }
    let expected_config = PathBuf::from(runner_temp).join("kattri-runtime-config");
    if env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).as_ref() != Some(&expected_config) {
        return Err("XDG_CONFIG_HOME must be the workflow's isolated runner directory".into());
    }
    Ok(())
}

fn profile_directory() -> Result<PathBuf, Box<dyn Error>> {
    let project = ProjectDirs::from("org", "Jellyfin", "Kattri")
        .ok_or("could not resolve the Kattri Linux config directory")?;
    let runner_temp =
        PathBuf::from(env::var_os("RUNNER_TEMP").ok_or("RUNNER_TEMP is unavailable")?);
    let expected_config = runner_temp.join("kattri-runtime-config");
    let config_dir = project.config_dir();
    if !config_dir.starts_with(&expected_config) {
        return Err("resolved config directory escaped the isolated runner config root".into());
    }
    Ok(config_dir.join("profiles"))
}

fn require_default_pointers(store: &ProfileStore) -> Result<(), Box<dyn Error>> {
    if store.default_profile() != "default" || store.active_profile() != "default" {
        return Err("the saved default and active pointers must remain default".into());
    }
    Ok(())
}

fn require_listed_profile(store: &ProfileStore, name: &str) -> Result<(), Box<dyn Error>> {
    if !store.names().iter().any(|profile| profile == name) {
        return Err("the acceptance profile is not indexed".into());
    }
    if !store.settings_path(name)?.is_file() {
        return Err("the acceptance profile settings file is missing".into());
    }
    Ok(())
}

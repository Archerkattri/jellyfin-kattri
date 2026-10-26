use std::{
    fs::{self, OpenOptions},
    io::Write,
};

use directories::ProjectDirs;
use uuid::Uuid;

/// Load the stable ID for this installation, creating one on first launch.
/// The identifier is metadata, not a credential; no password or access token
/// is stored beside it.
pub fn load_or_create() -> String {
    let generated = Uuid::new_v4();
    let Some(project_dirs) = ProjectDirs::from("org", "Jellyfin", "Kattri") else {
        return generated.to_string();
    };
    let path = project_dirs.config_local_dir().join("device-id");

    if let Some(existing) = read_id(&path) {
        return existing;
    }
    if let Some(parent) = path.parent()
        && fs::create_dir_all(parent).is_err()
    {
        return generated.to_string();
    }

    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut file) => {
            if writeln!(file, "{generated}")
                .and_then(|()| file.sync_all())
                .is_ok()
            {
                generated.to_string()
            } else {
                read_id(&path).unwrap_or_else(|| generated.to_string())
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            read_id(&path).unwrap_or_else(|| generated.to_string())
        }
        Err(_) => generated.to_string(),
    }
}

fn read_id(path: &std::path::Path) -> Option<String> {
    let value = fs::read_to_string(path).ok()?;
    let id = Uuid::parse_str(value.trim()).ok()?;
    Some(id.hyphenated().to_string())
}

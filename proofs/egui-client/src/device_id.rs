use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
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

/// Load the installation ID unless a debug-only per-process test override is present.
pub fn load_for_process() -> io::Result<String> {
    if let Some(id) = crate::worker::device_identity::from_environment()? {
        return Ok(id);
    }
    Ok(load_or_create())
}

fn read_id(path: &std::path::Path) -> Option<String> {
    let value = fs::read_to_string(path).ok()?;
    parse_id(&value)
}

fn parse_id(value: &str) -> Option<String> {
    let id = Uuid::parse_str(value.trim()).ok()?;
    Some(id.hyphenated().to_string())
}

#[cfg(test)]
mod tests {
    use super::parse_id;

    #[test]
    fn device_id_values_must_be_uuids_and_are_normalized() {
        assert_eq!(
            parse_id("ABCDEFAB-1234-4234-8234-ABCDEFABCDEF"),
            Some("abcdefab-1234-4234-8234-abcdefabcdef".into())
        );
        assert_eq!(parse_id("not-a-uuid"), None);
    }
}

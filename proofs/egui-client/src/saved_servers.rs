//! Servers offered on the sign-in screen, kept in a file in the user's config folder.
//!
//! The file holds a display name and a base address for each server. It never holds a
//! token, a password or a Quick Connect secret. Entries are validated by the client-core
//! server list, which also rejects plain HTTP to public hosts and addresses with
//! credentials or query strings.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use kattri_client_core::servers::{ServerEntry, ServerId, ServerList, ServerListError};
use uuid::Uuid;

const SERVERS_FILE_NAME: &str = "servers.json";

/// File that holds the saved servers, or `None` when no per-user config folder is known.
pub(crate) fn default_servers_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("org", "Jellyfin", "Kattri")
        .map(|dirs| dirs.config_dir().join(SERVERS_FILE_NAME))
}

pub(crate) struct SavedServers {
    path: Option<PathBuf>,
    list: ServerList,
    notice: Option<String>,
}

impl SavedServers {
    /// Read the saved list from `path`. A missing file is an empty list. A file that
    /// cannot be read is also an empty list, with a notice for the sign-in screen.
    pub(crate) fn open(path: Option<PathBuf>) -> Self {
        let mut notice = None;
        let list =
            match path.as_deref() {
                None => ServerList::new(),
                Some(file) => match fs::read_to_string(file) {
                    Ok(text) => match ServerList::decode(&text) {
                        Ok(list) => list,
                        Err(error) => {
                            tracing::warn!(%error, "saved server list could not be decoded");
                            notice = Some(match move_corrupt_aside(file) {
                            Ok(_) => "Saved server list was corrupt; a copy was kept aside.",
                            Err(_) => "Saved server list was corrupt and could not be moved aside.",
                        }.to_owned());
                            ServerList::new()
                        }
                    },
                    Err(error) if error.kind() == ErrorKind::NotFound => ServerList::new(),
                    Err(error) => {
                        tracing::warn!(%error, "saved server list could not be read");
                        notice = Some("Saved servers could not be read.".to_owned());
                        ServerList::new()
                    }
                },
            };
        Self { path, list, notice }
    }

    pub(crate) fn entries(&self) -> &[ServerEntry] {
        self.list.entries()
    }

    /// The server to resume a saved sign-in on: the default one, else the first.
    pub(crate) fn resume_candidate(&self) -> Option<&ServerEntry> {
        self.list
            .default_server()
            .or_else(|| self.list.entries().first())
    }

    pub(crate) fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// Save a server after a successful sign-in. A server already in the list, by its
    /// canonical address, is left as it is. A failure sets the notice instead.
    pub(crate) fn remember(
        &mut self,
        display_name: &str,
        base_url: &str,
        allow_insecure_remote_http: bool,
    ) -> Result<(), String> {
        let result = self.try_remember(display_name, base_url, allow_insecure_remote_http);
        if let Err(message) = &result {
            self.notice = Some(message.clone());
        }
        result
    }

    fn try_remember(
        &mut self,
        display_name: &str,
        base_url: &str,
        allow_insecure_remote_http: bool,
    ) -> Result<(), String> {
        let id = ServerId::parse(&Uuid::new_v4().hyphenated().to_string())
            .map_err(|error| error.to_string())?;
        let entry = ServerEntry::new(id, display_name, base_url, allow_insecure_remote_http)
            .map_err(|error| error.to_string())?;
        match self.list.add(entry) {
            Ok(()) => self.save(),
            Err(ServerListError::DuplicateBaseUrl) => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }

    fn save(&mut self) -> Result<(), String> {
        let Some(path) = self.path.as_deref() else {
            return Err("Saved servers are not stored in this session.".to_owned());
        };
        write_atomic(path, self.list.encode().as_bytes()).map_err(|error| {
            tracing::warn!(%error, "saved server list could not be written");
            "Saved servers could not be written.".to_owned()
        })
    }
}

/// Preserve malformed or unsupported data for recovery instead of overwriting it.
fn move_corrupt_aside(path: &Path) -> std::io::Result<PathBuf> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    let stem = path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("servers");
    let extension = path
        .extension()
        .and_then(|name| name.to_str())
        .unwrap_or("json");
    let aside = path.with_file_name(format!(
        "{stem}.corrupt-{timestamp}-{}.{}",
        Uuid::new_v4().simple(),
        extension
    ));
    fs::rename(path, &aside)?;
    Ok(aside)
}

/// Write through a temporary file in the same folder, then rename over the target.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::SavedServers;
    use std::path::PathBuf;
    use uuid::Uuid;

    fn temp_file() -> PathBuf {
        std::env::temp_dir()
            .join(format!("kattri-saved-servers-{}", Uuid::new_v4()))
            .join("servers.json")
    }

    #[test]
    fn a_new_server_is_saved_once_and_read_back() {
        let path = temp_file();
        let mut saved = SavedServers::open(Some(path.clone()));
        assert!(saved.entries().is_empty());
        saved
            .remember("Living room", "http://10.0.0.10:8096", false)
            .expect("first save succeeds");
        saved
            .remember("Living room", "http://10.0.0.10:8096/", false)
            .expect("repeat save is accepted");
        assert_eq!(saved.entries().len(), 1);

        let reopened = SavedServers::open(Some(path.clone()));
        assert_eq!(reopened.entries().len(), 1);
        assert_eq!(reopened.entries()[0].display_name(), "Living room");
        assert!(reopened.notice().is_none());
        if let Some(dir) = path.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn a_missing_folder_keeps_the_list_in_memory_and_reports_it() {
        let mut saved = SavedServers::open(None);
        assert!(saved.entries().is_empty());
        assert!(
            saved
                .remember("Lab", "http://10.0.0.5:8096", false)
                .is_err()
        );
        assert_eq!(saved.entries().len(), 1);
    }

    #[test]
    fn a_public_plain_http_address_is_refused() {
        let mut saved = SavedServers::open(None);
        assert!(
            saved
                .remember("Public", "http://example.com", false)
                .is_err()
        );
        assert!(saved.entries().is_empty());
    }

    #[test]
    fn a_corrupt_file_is_moved_aside_and_not_overwritten() {
        let path = temp_file();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create test directory");
        }
        std::fs::write(&path, b"not a server list").expect("write corrupt fixture");

        let saved = SavedServers::open(Some(path.clone()));

        assert!(saved.entries().is_empty());
        assert!(
            saved
                .notice()
                .is_some_and(|notice| notice.contains("copy was kept aside"))
        );
        assert!(!path.exists());
        let aside = std::fs::read_dir(path.parent().expect("test path has a parent"))
            .expect("read test directory")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|candidate| {
                candidate
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name.starts_with("servers.corrupt-") && name.ends_with(".json")
                    })
            })
            .expect("corrupt file is preserved with a timestamped name");
        assert_eq!(
            std::fs::read(aside).expect("read preserved fixture"),
            b"not a server list"
        );
        if let Some(dir) = path.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

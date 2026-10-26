//! The Discord Rich Presence plugin: playback notices in, presence frames out.
//!
//! The plugin never opens a socket. Each step goes through [`invoke`]: reading playback
//! needs `playback.read`, and every frame needs `presence.write`. A refused call sends
//! nothing and returns [`PluginError::PermissionDenied`].
//!
//! A transport failure is absorbed. Playback tracking continues, the connection is retried
//! after [`RETRY_BACKOFF`], and the current activity is sent once the connection is back.
//! Discord being absent must never stop playback reporting.

use std::time::Duration;

use crate::capability::{HostCall, invoke};
use crate::discord::presence::{PlaybackState, PresenceOptions, activity_for};
use crate::discord::transport::PresenceTransport;
use crate::discord::{
    Opcode, PresenceActivity, encode_frame, handshake_payload, set_activity_payload,
};
use crate::error::PluginError;
use crate::manifest::PluginManifest;
use crate::permission::PermissionGrants;

/// Minimum time between two progress-driven presence updates.
pub const PROGRESS_INTERVAL: Duration = Duration::from_secs(5);

/// Wait before the next connection attempt after a failure.
pub const RETRY_BACKOFF: Duration = Duration::from_secs(15);

/// Playback changes the plugin reacts to. None of them carries a URL, a token or a server
/// origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaybackNotice {
    /// The current item changed. Only the title is kept, and only shown if the user allows.
    ItemChanged {
        title: String,
    },
    Started {
        position_ms: u64,
    },
    /// Periodic state report. Updates the presence at most once per [`PROGRESS_INTERVAL`].
    Progress {
        position_ms: u64,
        paused: bool,
    },
    Paused {
        position_ms: u64,
    },
    Resumed {
        position_ms: u64,
    },
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Playhead {
    paused: bool,
    position_ms: u64,
}

/// Presence for one plugin instance. Generic over the transport so tests use the mock.
pub struct PresencePlugin<T: PresenceTransport> {
    manifest: PluginManifest,
    grants: PermissionGrants,
    transport: T,
    client_id: String,
    options: PresenceOptions,
    process_id: u32,
    title: String,
    /// `None` while nothing is playing.
    playhead: Option<Playhead>,
    /// The activity the connection last accepted. `None` means nothing is shown.
    shown: Option<PresenceActivity>,
    last_sent: Option<Duration>,
    retry_at: Option<Duration>,
    connected: bool,
    closed: bool,
    next_nonce: u64,
}

impl<T: PresenceTransport> PresencePlugin<T> {
    /// Build a plugin for `manifest`'s grants. `client_id` is the Discord application id,
    /// digits only; it is checked here so a bad id fails at setup, not on first playback.
    pub fn new(
        manifest: PluginManifest,
        grants: PermissionGrants,
        transport: T,
        client_id: &str,
        options: PresenceOptions,
    ) -> Result<Self, PluginError> {
        handshake_payload(client_id)?;
        Ok(Self {
            manifest,
            grants,
            transport,
            client_id: client_id.to_owned(),
            options,
            process_id: std::process::id(),
            title: String::new(),
            playhead: None,
            shown: None,
            last_sent: None,
            retry_at: None,
            connected: false,
            closed: false,
            next_nonce: 1,
        })
    }

    /// Whether the last connection attempt succeeded and no write has failed since.
    pub fn is_connected(&self) -> bool {
        self.connected
    }

    /// Apply one playback notice. `now` is a monotonic clock supplied by the host.
    ///
    /// Returns an error only for a refused host call. Transport failures are handled
    /// internally and return `Ok`.
    pub fn handle(&mut self, notice: PlaybackNotice, now: Duration) -> Result<(), PluginError> {
        if self.closed {
            return Ok(());
        }
        invoke(&self.manifest, &self.grants, &HostCall::PlaybackRead)?;

        let is_progress = matches!(notice, PlaybackNotice::Progress { .. });
        match notice {
            PlaybackNotice::ItemChanged { title } => self.title = title,
            PlaybackNotice::Started { position_ms } => {
                self.playhead = Some(Playhead {
                    paused: false,
                    position_ms,
                });
            }
            PlaybackNotice::Progress {
                position_ms,
                paused,
            } => {
                // A progress report with no prior start activates the presence too. That is
                // how a subscriber that joins mid-playback starts.
                self.playhead = Some(Playhead {
                    paused,
                    position_ms,
                });
            }
            PlaybackNotice::Paused { position_ms } => {
                self.playhead = Some(Playhead {
                    paused: true,
                    position_ms,
                });
            }
            PlaybackNotice::Resumed { position_ms } => {
                self.playhead = Some(Playhead {
                    paused: false,
                    position_ms,
                });
            }
            PlaybackNotice::Stopped => self.playhead = None,
        }

        if is_progress && self.progress_is_throttled(now) {
            return Ok(());
        }
        self.sync(now, false)
    }

    /// Clear the presence and close the connection. Call on app exit; `Drop` calls it too.
    ///
    /// It ignores the backoff and any failure, so exit never waits on Discord. It is safe
    /// to call twice.
    pub fn shutdown(&mut self, now: Duration) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.playhead = None;
        // A refused presence.write leaves the clear unsent. The grant decision stands.
        let _ = self.sync(now, true);
        self.transport.close();
        self.connected = false;
    }

    fn progress_is_throttled(&self, now: Duration) -> bool {
        self.last_sent
            .is_some_and(|sent| now.saturating_sub(sent) < PROGRESS_INTERVAL)
    }

    /// The activity that should be shown now, or `None` when nothing should be.
    fn desired(&self) -> Option<PresenceActivity> {
        let head = self.playhead?;
        let state = PlaybackState {
            title: self.title.clone(),
            paused: head.paused,
            position_ms: head.position_ms,
        };
        Some(activity_for(&state, self.options))
    }

    /// Bring the connection in line with the desired activity.
    ///
    /// `force` skips the connection backoff. Only shutdown uses it.
    fn sync(&mut self, now: Duration, force: bool) -> Result<(), PluginError> {
        let desired = self.desired();
        if desired == self.shown {
            return Ok(());
        }

        let activity = desired.clone().unwrap_or_default();
        let nonce = self.take_nonce();
        let payload = set_activity_payload(&activity, self.process_id, &nonce)?;
        let text = String::from_utf8_lossy(&payload).into_owned();
        invoke(
            &self.manifest,
            &self.grants,
            &HostCall::PresenceWrite { payload: text },
        )?;

        if !self.connected {
            if !force && self.retry_at.is_some_and(|at| now < at) {
                return Ok(());
            }
            if self.transport.connect(&self.client_id).is_err() {
                self.retry_at = Some(now.saturating_add(RETRY_BACKOFF));
                return Ok(());
            }
            self.connected = true;
            self.retry_at = None;
        }

        let frame = encode_frame(Opcode::Frame, &payload)?;
        self.last_sent = Some(now);
        if self.transport.write_frame(&frame).is_ok() {
            self.shown = desired;
        } else {
            self.connected = false;
            self.retry_at = Some(now.saturating_add(RETRY_BACKOFF));
        }
        Ok(())
    }

    fn take_nonce(&mut self) -> String {
        let nonce = format!("kattri-{}", self.next_nonce);
        self.next_nonce = self.next_nonce.wrapping_add(1);
        nonce
    }
}

impl<T: PresenceTransport> Drop for PresencePlugin<T> {
    fn drop(&mut self) {
        // Drop has no clock. The value is only bookkeeping: shutdown forces the send, and
        // nothing reads the retry time after it.
        self.shutdown(Duration::ZERO);
    }
}

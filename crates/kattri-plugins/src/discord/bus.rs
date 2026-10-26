//! Binds the presence plugin to the playback event bus.
//!
//! The bus carries no item title, so this binding never sends `ItemChanged`. The title keeps
//! its default and the presence shows "Watching" until a title source exists.
//!
//! The binding owns its [`Subscription`]. Each queued event is taken out of the queue
//! whatever the connection does, and a transport failure is absorbed inside the plugin. So a
//! failed Discord connection never makes the bus stop delivering.

use std::time::Duration;

use kattri_client_core::events::{PlaybackBusEvent, Subscription};

use crate::discord::plugin::{PlaybackNotice, PresencePlugin};
use crate::discord::transport::PresenceTransport;
use crate::error::PluginError;

/// A presence plugin fed by one bus subscription.
pub struct PresenceBinding<T: PresenceTransport> {
    subscription: Subscription,
    plugin: PresencePlugin<T>,
    /// Last known position and play state. The bus's pause and resume events carry no
    /// position, and a progress notice needs the play state.
    position_ms: u64,
    paused: bool,
}

impl<T: PresenceTransport> PresenceBinding<T> {
    pub fn new(subscription: Subscription, plugin: PresencePlugin<T>) -> Self {
        Self {
            subscription,
            plugin,
            position_ms: 0,
            paused: false,
        }
    }

    /// The plugin this binding feeds.
    pub fn plugin(&self) -> &PresencePlugin<T> {
        &self.plugin
    }

    /// Apply every queued event, oldest first. `now` is the monotonic clock the plugin uses.
    ///
    /// The whole queue is taken even after a refused host call. The first refusal is returned.
    pub fn pump(&mut self, now: Duration) -> Result<(), PluginError> {
        let mut result = Ok(());
        while let Some(event) = self.subscription.try_recv() {
            let Some(notice) = self.notice_for(event) else {
                continue;
            };
            let applied = self.plugin.handle(notice, now);
            if result.is_ok() {
                result = applied;
            }
        }
        result
    }

    /// Clear the presence and close the connection. Call on app exit. The plugin's `Drop`
    /// does the same, but only when the owner drops the binding.
    pub fn shutdown(&mut self, now: Duration) {
        self.plugin.shutdown(now);
    }

    fn notice_for(&mut self, event: PlaybackBusEvent) -> Option<PlaybackNotice> {
        match event {
            PlaybackBusEvent::Started => {
                self.position_ms = 0;
                self.paused = false;
                Some(PlaybackNotice::Started { position_ms: 0 })
            }
            PlaybackBusEvent::Position { position_ms } => {
                self.position_ms = position_ms;
                Some(PlaybackNotice::Progress {
                    position_ms,
                    paused: self.paused,
                })
            }
            PlaybackBusEvent::Snapshot {
                position_ms,
                paused,
                ..
            } => {
                self.position_ms = position_ms;
                self.paused = paused;
                Some(PlaybackNotice::Progress {
                    position_ms,
                    paused,
                })
            }
            PlaybackBusEvent::Paused => {
                self.paused = true;
                Some(PlaybackNotice::Paused {
                    position_ms: self.position_ms,
                })
            }
            PlaybackBusEvent::Resumed => {
                self.paused = false;
                Some(PlaybackNotice::Resumed {
                    position_ms: self.position_ms,
                })
            }
            PlaybackBusEvent::Stopped => Some(PlaybackNotice::Stopped),
            PlaybackBusEvent::TrackChanged { .. } | PlaybackBusEvent::Error { .. } => None,
        }
    }
}

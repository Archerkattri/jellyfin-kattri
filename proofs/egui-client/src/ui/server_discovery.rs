//! A single bounded LAN scan. Its blocking UDP work runs outside the UI thread.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};

use jellyfin_api::{DiscoveryCandidate, DiscoveryError, DiscoveryOptions, discover_lan_servers};
use kattri_i18n::ids;

use super::ui_text;

type ScanResult = Result<Vec<DiscoveryCandidate>, DiscoveryError>;

#[derive(Default)]
struct ScanState {
    receiver: Option<mpsc::Receiver<ScanResult>>,
    cancelled: Arc<AtomicBool>,
    candidates: Vec<DiscoveryCandidate>,
    notice: Option<&'static str>,
}

impl ScanState {
    fn poll(&mut self) {
        let Some(receiver) = self.receiver.as_ref() else {
            return;
        };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => {
                Err(DiscoveryError::Socket(std::io::ErrorKind::BrokenPipe))
            }
        };
        self.receiver = None;
        match result {
            Ok(candidates) => {
                self.notice = candidates
                    .is_empty()
                    .then_some(ids::CONNECT_DISCOVERY_EMPTY);
                self.candidates = candidates;
            }
            Err(DiscoveryError::Cancelled) => self.notice = Some(ids::CONNECT_DISCOVERY_CANCELLED),
            Err(_) => self.notice = Some(ids::CONNECT_DISCOVERY_FAILED),
        }
    }

    fn start(&mut self, ctx: egui::Context) {
        if self.receiver.is_some() {
            return;
        }
        let (sender, receiver) = mpsc::sync_channel(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let spawned = std::thread::Builder::new()
            .name("kattri-lan-discovery".to_owned())
            .spawn(move || {
                let result = discover_lan_servers(DiscoveryOptions::default(), &worker_cancelled);
                let _ = sender.send(result);
                ctx.request_repaint();
            });
        match spawned {
            Ok(_) => {
                self.receiver = Some(receiver);
                self.cancelled = cancelled;
                self.notice = None;
            }
            Err(_) => self.notice = Some(ids::CONNECT_DISCOVERY_FAILED),
        }
    }
}

impl Drop for ScanState {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

/// Returns only an explicitly selected address. Normal connection checks still apply.
pub(super) fn show(ui: &mut egui::Ui) -> Option<String> {
    let id = egui::Id::unique("kattri-lan-discovery-state");
    let state = ui.ctx().data_mut(|data| {
        data.get_temp_mut_or_default::<Arc<Mutex<ScanState>>>(id)
            .clone()
    });
    let mut state = state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state.poll();
    ui.horizontal(|ui| {
        if state.receiver.is_some() {
            ui.spinner();
            ui.label(ui_text(ids::CONNECT_DISCOVERY_SCANNING));
            if ui.button(ui_text(ids::CONNECT_DISCOVERY_CANCEL)).clicked() {
                state.cancelled.store(true, Ordering::Release);
            }
        } else if ui.button(ui_text(ids::CONNECT_DISCOVERY_START)).clicked() {
            state.start(ui.ctx().clone());
        }
    });
    if let Some(notice) = state.notice {
        ui.label(ui_text(notice));
    }
    let mut selected = None;
    for candidate in &state.candidates {
        let host = candidate.address().host_str().unwrap_or_default();
        if ui
            .button(format!("{} ({host})", candidate.name()))
            .clicked()
        {
            selected = Some(candidate.address().to_string());
        }
    }
    selected
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_errors_leave_the_form_usable_and_allow_retry() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let mut state = ScanState {
            receiver: Some(receiver),
            cancelled: Arc::new(AtomicBool::new(false)),
            candidates: Vec::new(),
            notice: None,
        };
        state.poll();
        assert!(state.receiver.is_some(), "a pending scan stays pending");
        sender
            .send(Err(DiscoveryError::Cancelled))
            .expect("receiver alive");
        state.poll();
        assert!(state.receiver.is_none());
        assert_eq!(state.notice, Some(ids::CONNECT_DISCOVERY_CANCELLED));
        let (sender, receiver) = mpsc::sync_channel(1);
        state.receiver = Some(receiver);
        sender.send(Ok(Vec::new())).expect("receiver alive");
        state.poll();
        assert_eq!(state.notice, Some(ids::CONNECT_DISCOVERY_EMPTY));
    }

    #[test]
    fn dropping_scan_state_cancels_its_worker_without_waiting() {
        let state = ScanState::default();
        let cancelled = Arc::clone(&state.cancelled);
        drop(state);
        assert!(cancelled.load(Ordering::Acquire));
    }
}

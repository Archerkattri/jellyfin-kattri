//! Native playback sleep and idle inhibition backends.

#[cfg(any(target_os = "macos", target_os = "linux"))]
use crate::PowerInhibitor;
#[cfg(target_os = "linux")]
use crate::PowerStatus;
#[cfg(any(target_os = "macos", target_os = "linux", test))]
use crate::{PlatformError, PowerGuard};

#[cfg(any(target_os = "macos", test))]
const FAILURE_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_secs(5);

#[cfg(any(target_os = "macos", test))]
#[derive(Debug, Default)]
struct FailureBackoff {
    retry_at: Option<std::time::Instant>,
    last_error: Option<PlatformError>,
}

#[cfg(any(target_os = "macos", test))]
impl FailureBackoff {
    fn run(
        &mut self,
        now: std::time::Instant,
        operation: impl FnOnce() -> Result<PowerGuard, PlatformError>,
    ) -> Result<PowerGuard, PlatformError> {
        if self.retry_at.is_some_and(|retry_at| now < retry_at) {
            return Err(self.last_error.clone().unwrap_or_else(|| {
                PlatformError::Failed("power inhibition retry is deferred".to_owned())
            }));
        }

        match operation() {
            Ok(guard) => {
                self.retry_at = None;
                self.last_error = None;
                Ok(guard)
            }
            Err(error) => {
                self.retry_at = now.checked_add(FAILURE_RETRY_BACKOFF);
                self.last_error = Some(error.clone());
                Err(error)
            }
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
const MAX_REASON_UTF16_UNITS: usize = 128;

#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn bounded_reason(reason: &str) -> Result<String, PlatformError> {
    if reason.contains('\0') {
        return Err(PlatformError::Failed(
            "inhibition reason contains a NUL character".to_owned(),
        ));
    }
    let mut end = 0;
    let mut utf16_units = 0;
    for (byte_index, character) in reason.char_indices() {
        let character_units = character.len_utf16();
        if utf16_units + character_units > MAX_REASON_UTF16_UNITS {
            break;
        }
        utf16_units += character_units;
        end = byte_index + character.len_utf8();
    }
    Ok(reason[..end].to_owned())
}

#[cfg(target_os = "macos")]
#[derive(Debug, Default)]
pub(super) struct MacOsPowerInhibitor {
    failures: FailureBackoff,
}

#[cfg(target_os = "macos")]
impl PowerInhibitor for MacOsPowerInhibitor {
    fn inhibit_sleep(&mut self, reason: &str) -> Result<PowerGuard, PlatformError> {
        self.failures.run(std::time::Instant::now(), || {
            let reason = bounded_reason(reason)?;
            let assertion_ids = acquire_assertion_pair(
                |kind| create_macos_assertion(kind, &reason),
                release_macos_assertion,
            )
            .map_err(|(kind, failure)| match failure {
                MacAssertionFailure::StringCreation => PlatformError::Failed(format!(
                    "Core Foundation could not create the {} name",
                    kind.description()
                )),
                MacAssertionFailure::Native(code) => PlatformError::Failed(format!(
                    "IOPMAssertionCreateWithName for {} failed with IOReturn {code:#x}",
                    kind.description()
                )),
            })?;

            Ok(PowerGuard::new(move || {
                for assertion_id in assertion_ids {
                    release_macos_assertion(assertion_id);
                }
            }))
        })
    }
}

#[cfg(any(target_os = "macos", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AssertionKind {
    SystemSleep,
    DisplaySleep,
}

#[cfg(any(target_os = "macos", test))]
impl AssertionKind {
    #[cfg(target_os = "macos")]
    fn type_name(self) -> &'static str {
        match self {
            Self::SystemSleep => "PreventUserIdleSystemSleep",
            Self::DisplaySleep => "PreventUserIdleDisplaySleep",
        }
    }

    #[cfg(target_os = "macos")]
    fn description(self) -> &'static str {
        match self {
            Self::SystemSleep => "system sleep assertion",
            Self::DisplaySleep => "display sleep assertion",
        }
    }
}

#[cfg(target_os = "macos")]
enum MacAssertionFailure {
    StringCreation,
    Native(i32),
}

/// Roll back the first assertion if the second native request fails.
#[cfg(any(target_os = "macos", test))]
fn acquire_assertion_pair<E>(
    mut create: impl FnMut(AssertionKind) -> Result<u32, E>,
    mut release: impl FnMut(u32),
) -> Result<[u32; 2], (AssertionKind, E)> {
    let system =
        create(AssertionKind::SystemSleep).map_err(|error| (AssertionKind::SystemSleep, error))?;
    match create(AssertionKind::DisplaySleep) {
        Ok(display) => Ok([system, display]),
        Err(error) => {
            release(system);
            Err((AssertionKind::DisplaySleep, error))
        }
    }
}

#[cfg(target_os = "macos")]
struct CfString(*const std::ffi::c_void);

#[cfg(target_os = "macos")]
impl CfString {
    fn new(value: &str) -> Result<Self, PlatformError> {
        const UTF8_ENCODING: u32 = 0x0800_0100;
        let byte_len = isize::try_from(value.len())
            .map_err(|_| PlatformError::Failed("Core Foundation string is too large".to_owned()))?;

        // SAFETY: `value` remains alive for the call and its byte length is explicit. A null
        // allocator requests Core Foundation's default allocator.
        let string = unsafe {
            CFStringCreateWithBytes(std::ptr::null(), value.as_ptr(), byte_len, UTF8_ENCODING, 0)
        };
        if string.is_null() {
            return Err(PlatformError::Failed(
                "Core Foundation could not create an inhibition string".to_owned(),
            ));
        }
        Ok(Self(string))
    }
}

#[cfg(target_os = "macos")]
impl Drop for CfString {
    fn drop(&mut self) {
        // SAFETY: `CfString::new` stores a +1 Core Foundation object exactly once.
        unsafe { CFRelease(self.0) };
    }
}

#[cfg(target_os = "macos")]
fn create_macos_assertion(kind: AssertionKind, reason: &str) -> Result<u32, MacAssertionFailure> {
    let assertion_type =
        CfString::new(kind.type_name()).map_err(|_| MacAssertionFailure::StringCreation)?;
    let name = CfString::new(reason).map_err(|_| MacAssertionFailure::StringCreation)?;
    let mut assertion_id = 0;

    // SAFETY: both Core Foundation strings live through the call and `assertion_id` is a valid
    // writable output pointer. IOPMAssertionLevelOn is the documented level value 255.
    let result =
        unsafe { IOPMAssertionCreateWithName(assertion_type.0, 255, name.0, &mut assertion_id) };
    if result == 0 {
        Ok(assertion_id)
    } else {
        Err(MacAssertionFailure::Native(result))
    }
}

#[cfg(target_os = "macos")]
fn release_macos_assertion(assertion_id: u32) {
    // SAFETY: the ID came from a successful IOPMAssertionCreateWithName call and is released once.
    let _ = unsafe { IOPMAssertionRelease(assertion_id) };
}

#[cfg(target_os = "macos")]
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithBytes(
        allocator: *const std::ffi::c_void,
        bytes: *const u8,
        num_bytes: isize,
        encoding: u32,
        is_external_representation: u8,
    ) -> *const std::ffi::c_void;
    fn CFRelease(value: *const std::ffi::c_void);
}

#[cfg(target_os = "macos")]
#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMAssertionCreateWithName(
        assertion_type: *const std::ffi::c_void,
        assertion_level: u32,
        assertion_name: *const std::ffi::c_void,
        assertion_id: *mut u32,
    ) -> i32;
    fn IOPMAssertionRelease(assertion_id: u32) -> i32;
}

#[cfg(target_os = "linux")]
pub(super) struct LinuxPowerInhibitor {
    commands: Option<async_channel::Sender<WorkerCommand>>,
    startup_error: Option<PlatformError>,
    admission: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(target_os = "linux")]
const MAX_LINUX_INHIBITORS: usize = 16;

#[cfg(target_os = "linux")]
impl std::fmt::Debug for LinuxPowerInhibitor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinuxPowerInhibitor")
            .field("worker_available", &self.commands.is_some())
            .finish_non_exhaustive()
    }
}

#[cfg(target_os = "linux")]
impl Default for LinuxPowerInhibitor {
    fn default() -> Self {
        let admission = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (commands, receiver) = async_channel::bounded(MAX_LINUX_INHIBITORS);
        match std::thread::Builder::new()
            .name("kattri-power-inhibit".to_owned())
            .spawn(move || {
                let executor = async_executor::LocalExecutor::new();
                async_io::block_on(executor.run(power_worker(receiver, &executor)));
            }) {
            Ok(worker) => {
                // The worker exits after the backend and all outstanding guards are dropped.
                drop(worker);
                Self {
                    commands: Some(commands),
                    startup_error: None,
                    admission,
                }
            }
            Err(error) => Self {
                commands: None,
                startup_error: Some(PlatformError::Failed(format!(
                    "could not start Linux inhibition worker: {error}"
                ))),
                admission,
            },
        }
    }
}

#[cfg(target_os = "linux")]
impl PowerInhibitor for LinuxPowerInhibitor {
    fn inhibit_sleep(&mut self, reason: &str) -> Result<PowerGuard, PlatformError> {
        let reason = bounded_reason(reason)?;
        let commands = self.commands.as_ref().ok_or_else(|| {
            self.startup_error.clone().unwrap_or_else(|| {
                PlatformError::Failed("Linux inhibition worker is unavailable".to_owned())
            })
        })?;
        let permit = AdmissionPermit::try_acquire(std::sync::Arc::clone(&self.admission))
            .ok_or_else(|| {
                PlatformError::Failed("Linux inhibition worker is at capacity".to_owned())
            })?;

        let status = std::sync::Arc::new(std::sync::Mutex::new(PowerStatus::Pending));
        let (cancel, cancellation) = async_channel::bounded(1);
        let command = WorkerCommand {
            request: WorkerRequest {
                reason,
                status: std::sync::Arc::clone(&status),
                cancellation,
            },
            _permit: permit,
        };
        commands.try_send(command).map_err(|error| match error {
            async_channel::TrySendError::Full(_) => {
                PlatformError::Failed("Linux inhibition worker queue is full".to_owned())
            }
            async_channel::TrySendError::Closed(_) => {
                PlatformError::Failed("Linux inhibition worker has stopped".to_owned())
            }
        })?;

        Ok(pending_power_guard(cancel, status))
    }
}

#[cfg(target_os = "linux")]
struct WorkerCommand {
    request: WorkerRequest,
    _permit: AdmissionPermit,
}

#[cfg(target_os = "linux")]
struct WorkerRequest {
    reason: String,
    status: std::sync::Arc<std::sync::Mutex<PowerStatus>>,
    cancellation: async_channel::Receiver<()>,
}

#[cfg(target_os = "linux")]
struct AdmissionPermit(std::sync::Arc<std::sync::atomic::AtomicUsize>);

#[cfg(target_os = "linux")]
impl AdmissionPermit {
    fn try_acquire(admission: std::sync::Arc<std::sync::atomic::AtomicUsize>) -> Option<Self> {
        let mut admitted = admission.load(std::sync::atomic::Ordering::Acquire);
        loop {
            if admitted >= MAX_LINUX_INHIBITORS {
                return None;
            }
            match admission.compare_exchange_weak(
                admitted,
                admitted + 1,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            ) {
                Ok(_) => return Some(Self(admission)),
                Err(current) => admitted = current,
            }
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for AdmissionPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

#[cfg(target_os = "linux")]
struct RequestStatusOnExit(std::sync::Arc<std::sync::Mutex<PowerStatus>>);

#[cfg(target_os = "linux")]
impl Drop for RequestStatusOnExit {
    fn drop(&mut self) {
        let mut status = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *status == PowerStatus::Pending || *status == PowerStatus::Active {
            *status = PowerStatus::Failed(PlatformError::Failed(
                "Linux inhibition worker stopped unexpectedly".to_owned(),
            ));
        }
    }
}

#[cfg(target_os = "linux")]
struct CompletionNotify {
    sender: async_channel::Sender<AdmissionPermit>,
    permit: Option<AdmissionPermit>,
}

#[cfg(target_os = "linux")]
impl Drop for CompletionNotify {
    fn drop(&mut self) {
        if let Some(permit) = self.permit.take() {
            let _ = self.sender.try_send(permit);
        }
    }
}

#[cfg(target_os = "linux")]
enum WorkerEvent {
    Command(Result<WorkerCommand, async_channel::RecvError>),
    Completed(Result<AdmissionPermit, async_channel::RecvError>),
}

#[cfg(target_os = "linux")]
async fn power_worker(
    commands: async_channel::Receiver<WorkerCommand>,
    executor: &async_executor::LocalExecutor<'_>,
) {
    let (completed_tx, completed_rx) = async_channel::bounded(MAX_LINUX_INHIBITORS);
    let mut commands_open = true;
    let mut active_requests: usize = 0;
    while commands_open || active_requests > 0 {
        let event = if commands_open && active_requests > 0 {
            futures_lite::future::or(
                async { WorkerEvent::Command(commands.recv().await) },
                async { WorkerEvent::Completed(completed_rx.recv().await) },
            )
            .await
        } else if commands_open {
            WorkerEvent::Command(commands.recv().await)
        } else {
            WorkerEvent::Completed(completed_rx.recv().await)
        };

        match event {
            WorkerEvent::Command(Ok(command)) => {
                active_requests += 1;
                let WorkerCommand { request, _permit } = command;
                let completed = completed_tx.clone();
                executor
                    .spawn(async move {
                        let _completion = CompletionNotify {
                            sender: completed,
                            permit: Some(_permit),
                        };
                        run_power_request(request).await;
                    })
                    .detach();
            }
            WorkerEvent::Command(Err(_)) => commands_open = false,
            WorkerEvent::Completed(Ok(_permit)) => {
                active_requests = active_requests.saturating_sub(1);
            }
            WorkerEvent::Completed(Err(_)) => break,
        }
    }
}

#[cfg(target_os = "linux")]
async fn run_power_request(command: WorkerRequest) {
    let WorkerRequest {
        reason,
        status,
        cancellation,
    } = command;
    let _status_on_exit = RequestStatusOnExit(std::sync::Arc::clone(&status));
    let acquired = acquire_until_cancel(acquire_linux_lease(&reason), &cancellation).await;
    let Some(acquired) = acquired else {
        set_status_if_pending(&status, PowerStatus::Released);
        return;
    };

    let lease = match acquired {
        Ok(Ok(lease)) => lease,
        Ok(Err(error)) => {
            set_status_if_pending(&status, PowerStatus::Failed(error));
            return;
        }
        Err(_) => {
            set_status_if_pending(
                &status,
                PowerStatus::Failed(PlatformError::Failed(
                    "Linux power inhibition acquisition timed out".to_owned(),
                )),
            );
            return;
        }
    };

    if !set_status_if_pending(&status, PowerStatus::Active) {
        lease.release().await;
        return;
    }

    wait_for_cancellation(&cancellation).await;
    lease.release().await;
    set_status_if_pending(&status, PowerStatus::Released);
}

#[cfg(target_os = "linux")]
async fn acquire_until_cancel<T>(
    acquisition: impl std::future::Future<Output = Result<T, PlatformError>>,
    cancellation: &async_channel::Receiver<()>,
) -> Option<Result<Result<T, PlatformError>, ()>> {
    let bounded_acquisition = futures_lite::future::or(async { Ok(acquisition.await) }, async {
        async_io::Timer::after(ACQUISITION_TIMEOUT).await;
        Err(())
    });
    futures_lite::future::or(async { Some(bounded_acquisition.await) }, async {
        let _ = cancellation.recv().await;
        None
    })
    .await
}

#[cfg(target_os = "linux")]
async fn wait_for_cancellation(cancellation: &async_channel::Receiver<()>) {
    let _ = cancellation.recv().await;
}

#[cfg(target_os = "linux")]
fn set_status_if_pending(status: &std::sync::Mutex<PowerStatus>, next: PowerStatus) -> bool {
    let mut status = status
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if *status == PowerStatus::Pending {
        *status = next;
        true
    } else {
        false
    }
}

#[cfg(target_os = "linux")]
fn pending_power_guard(
    cancel: async_channel::Sender<()>,
    status: std::sync::Arc<std::sync::Mutex<PowerStatus>>,
) -> PowerGuard {
    let release_status = std::sync::Arc::clone(&status);
    PowerGuard::with_status(
        move || {
            *release_status
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = PowerStatus::Released;
            let _ = cancel.try_send(());
        },
        status,
    )
}

#[cfg(target_os = "linux")]
const APPLICATION_NAME: &str = "Jellyfin Kattri";

#[cfg(target_os = "linux")]
const LOGIND_INHIBITION: &str = "systemd-logind sleep/idle inhibition";

#[cfg(target_os = "linux")]
const SCREEN_SAVER_INHIBITION: &str = "freedesktop ScreenSaver idle inhibition";

#[cfg(target_os = "linux")]
const DBUS_METHOD_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

#[cfg(target_os = "linux")]
const ACQUISITION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

#[cfg(target_os = "linux")]
async fn acquire_linux_lease(reason: &str) -> Result<LinuxPowerLease, PlatformError> {
    let system_bus = zbus::connection::Builder::system()
        .map_err(|error| map_dbus_error(error, LOGIND_INHIBITION))?
        .method_timeout(DBUS_METHOD_TIMEOUT)
        .build()
        .await
        .map_err(|error| map_dbus_error(error, LOGIND_INHIBITION))?;
    let login1 = zbus::Proxy::new(
        &system_bus,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )
    .await
    .map_err(|error| map_dbus_error(error, LOGIND_INHIBITION))?;
    let sleep_fd: zbus::zvariant::OwnedFd = login1
        .call(
            "Inhibit",
            &("sleep:idle", APPLICATION_NAME, reason, "block"),
        )
        .await
        .map_err(|error| map_dbus_error(error, LOGIND_INHIBITION))?;
    drop(login1);
    drop(system_bus);

    let session_bus = zbus::connection::Builder::session()
        .map_err(|error| map_dbus_error(error, SCREEN_SAVER_INHIBITION))?
        .method_timeout(DBUS_METHOD_TIMEOUT)
        .build()
        .await
        .map_err(|error| map_dbus_error(error, SCREEN_SAVER_INHIBITION))?;
    let screen_saver = zbus::Proxy::new(
        &session_bus,
        "org.freedesktop.ScreenSaver",
        "/org/freedesktop/ScreenSaver",
        "org.freedesktop.ScreenSaver",
    )
    .await
    .map_err(|error| map_dbus_error(error, SCREEN_SAVER_INHIBITION))?;
    let cookie: u32 = screen_saver
        .call("Inhibit", &(APPLICATION_NAME, reason))
        .await
        .map_err(|error| map_dbus_error(error, SCREEN_SAVER_INHIBITION))?;

    Ok(LinuxPowerLease {
        session_bus,
        cookie,
        _sleep_fd: sleep_fd,
    })
}

#[cfg(target_os = "linux")]
struct LinuxPowerLease {
    session_bus: zbus::Connection,
    cookie: u32,
    // logind releases this inhibitor when the last copy of its returned fd is closed.
    _sleep_fd: zbus::zvariant::OwnedFd,
}

#[cfg(target_os = "linux")]
impl LinuxPowerLease {
    async fn release(self) {
        let Self {
            session_bus,
            cookie,
            _sleep_fd,
        } = self;
        let release_request = async {
            if let Ok(screen_saver) = zbus::Proxy::new(
                &session_bus,
                "org.freedesktop.ScreenSaver",
                "/org/freedesktop/ScreenSaver",
                "org.freedesktop.ScreenSaver",
            )
            .await
            {
                // The worker never waits for the desktop service's method reply.
                let _ = screen_saver.call_noreply("UnInhibit", &(cookie,)).await;
            }
        };
        let bounded_release = futures_lite::future::or(
            async {
                release_request.await;
                true
            },
            async {
                async_io::Timer::after(DBUS_METHOD_TIMEOUT).await;
                false
            },
        );
        let _ = bounded_release.await;
        drop(_sleep_fd);
        drop(session_bus);
    }
}

#[cfg(target_os = "linux")]
fn map_dbus_error(error: zbus::Error, service: &'static str) -> PlatformError {
    match error {
        zbus::Error::MethodError(name, details, _) => {
            map_method_error_name(name.as_str(), service, details.as_deref())
        }
        error => PlatformError::Failed(format!("{service} request failed: {error}")),
    }
}

#[cfg(any(target_os = "linux", test))]
fn map_method_error_name(
    name: &str,
    service: &'static str,
    details: Option<&str>,
) -> PlatformError {
    match name {
        "org.freedesktop.DBus.Error.ServiceUnknown"
        | "org.freedesktop.DBus.Error.NameHasNoOwner"
        | "org.freedesktop.DBus.Error.UnknownMethod" => PlatformError::Unsupported(service),
        "org.freedesktop.DBus.Error.AccessDenied" | "org.freedesktop.DBus.Error.NotAuthorized" => {
            PlatformError::Denied(
                details
                    .unwrap_or("the desktop service denied the inhibition request")
                    .to_owned(),
            )
        }
        _ => PlatformError::Failed(format!(
            "{service} returned {name}: {}",
            details.unwrap_or("no diagnostic was provided")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inhibition_reason_is_bounded_on_character_boundaries() {
        let reason = "🎬".repeat(MAX_REASON_UTF16_UNITS + 10);
        let bounded = bounded_reason(&reason).expect("valid reason");
        assert_eq!(bounded.encode_utf16().count(), MAX_REASON_UTF16_UNITS);
        assert_eq!(bounded.chars().count(), MAX_REASON_UTF16_UNITS / 2);
    }

    #[test]
    fn inhibition_reason_rejects_nul() {
        assert!(matches!(
            bounded_reason("playing\0video"),
            Err(PlatformError::Failed(_))
        ));
    }

    #[test]
    fn failed_display_assertion_rolls_back_the_system_assertion() {
        let calls = std::cell::RefCell::new(Vec::new());
        let result = acquire_assertion_pair(
            |kind| {
                calls.borrow_mut().push(("create", kind));
                match kind {
                    AssertionKind::SystemSleep => Ok(41),
                    AssertionKind::DisplaySleep => Err("display unavailable"),
                }
            },
            |id| {
                calls.borrow_mut().push((
                    "release",
                    if id == 41 {
                        AssertionKind::SystemSleep
                    } else {
                        AssertionKind::DisplaySleep
                    },
                ))
            },
        );

        assert_eq!(
            result,
            Err((AssertionKind::DisplaySleep, "display unavailable"))
        );
        assert_eq!(
            calls.into_inner(),
            [
                ("create", AssertionKind::SystemSleep),
                ("create", AssertionKind::DisplaySleep),
                ("release", AssertionKind::SystemSleep),
            ]
        );
    }

    #[test]
    fn dropping_a_power_guard_releases_once() {
        let releases = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let release_count = std::sync::Arc::clone(&releases);
        let guard = PowerGuard::new(move || {
            release_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        drop(guard);

        assert_eq!(releases.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn dbus_service_errors_are_typed() {
        assert_eq!(
            map_method_error_name(
                "org.freedesktop.DBus.Error.ServiceUnknown",
                "systemd-logind sleep/idle inhibition",
                None
            ),
            PlatformError::Unsupported("systemd-logind sleep/idle inhibition")
        );
        assert_eq!(
            map_method_error_name(
                "org.freedesktop.DBus.Error.AccessDenied",
                "freedesktop ScreenSaver idle inhibition",
                Some("policy denied")
            ),
            PlatformError::Denied("policy denied".to_owned())
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pending_linux_guard_reports_state_and_cancels_on_drop() {
        let status = std::sync::Arc::new(std::sync::Mutex::new(PowerStatus::Pending));
        let (cancel, cancellation) = async_channel::bounded(1);
        let guard = pending_power_guard(cancel, std::sync::Arc::clone(&status));

        assert_eq!(guard.status(), PowerStatus::Pending);
        assert!(set_status_if_pending(&status, PowerStatus::Active));
        assert_eq!(guard.status(), PowerStatus::Active);

        drop(guard);
        assert_eq!(status.lock().unwrap().clone(), PowerStatus::Released);
        assert_eq!(cancellation.try_recv(), Ok(()));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_admission_caps_queued_and_active_requests() {
        let admission = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let permits = (0..MAX_LINUX_INHIBITORS)
            .map(|_| {
                AdmissionPermit::try_acquire(std::sync::Arc::clone(&admission))
                    .expect("admission slot")
            })
            .collect::<Vec<_>>();

        assert!(AdmissionPermit::try_acquire(std::sync::Arc::clone(&admission)).is_none());
        drop(permits);
        assert!(AdmissionPermit::try_acquire(admission).is_some());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cancellation_drops_an_in_flight_acquisition_future() {
        struct NeverCompletes(std::sync::Arc<std::sync::atomic::AtomicBool>);

        impl std::future::Future for NeverCompletes {
            type Output = Result<(), PlatformError>;

            fn poll(
                self: std::pin::Pin<&mut Self>,
                _context: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Self::Output> {
                std::task::Poll::Pending
            }
        }

        impl Drop for NeverCompletes {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }

        let acquisition_dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (cancel, cancellation) = async_channel::bounded(1);
        let outcome = async_io::block_on(async {
            futures_lite::future::zip(
                acquire_until_cancel(
                    NeverCompletes(std::sync::Arc::clone(&acquisition_dropped)),
                    &cancellation,
                ),
                async {
                    async_io::Timer::after(std::time::Duration::from_millis(10)).await;
                    let _ = cancel.try_send(());
                },
            )
            .await
            .0
        });

        assert!(outcome.is_none());
        assert!(acquisition_dropped.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn failed_acquisition_is_not_retried_until_backoff_expires() {
        let now = std::time::Instant::now();
        let mut backoff = FailureBackoff::default();
        let mut attempts = 0;

        assert!(matches!(
            backoff.run(now, || {
                attempts += 1;
                Err(PlatformError::Unsupported("test power service"))
            }),
            Err(PlatformError::Unsupported("test power service"))
        ));
        assert!(matches!(
            backoff.run(now + std::time::Duration::from_secs(1), || {
                attempts += 1;
                Ok(PowerGuard::new(|| {}))
            }),
            Err(PlatformError::Unsupported("test power service"))
        ));
        assert_eq!(attempts, 1);

        let guard = backoff
            .run(now + FAILURE_RETRY_BACKOFF, || {
                attempts += 1;
                Ok(PowerGuard::new(|| {}))
            })
            .expect("retry after backoff");
        assert_eq!(attempts, 2);
        drop(guard);
    }
}

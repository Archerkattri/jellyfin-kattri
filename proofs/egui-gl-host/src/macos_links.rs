//! Bounded intake for macOS `application:openURLs:` callbacks.

use std::cell::Cell;
use std::rc::Rc;

use kattri_client_core::deep_link::DeepLinkIntent;

/// Admission state prevents URL callbacks from creating an unbounded winit user-event queue.
#[derive(Clone)]
pub(crate) struct MacLinkGate(Rc<Cell<GateState>>);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GateState {
    StartupIntentPending,
    BeforeWindow,
    Ready,
    QueuedBeforeWindow,
    QueuedAfterWindow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MacLinkReject {
    InvalidUrl,
    TooManyUrls,
    StartupIntentPending,
    EventPending,
    #[cfg(target_os = "macos")]
    EventLoopClosed,
}

impl MacLinkGate {
    pub(crate) fn new(startup_intent_pending: bool) -> Self {
        let state = if startup_intent_pending {
            GateState::StartupIntentPending
        } else {
            GateState::BeforeWindow
        };
        Self(Rc::new(Cell::new(state)))
    }

    fn reserve(&self) -> Result<(), MacLinkReject> {
        let queued = match self.0.get() {
            GateState::BeforeWindow => GateState::QueuedBeforeWindow,
            GateState::Ready => GateState::QueuedAfterWindow,
            GateState::StartupIntentPending => return Err(MacLinkReject::StartupIntentPending),
            GateState::QueuedBeforeWindow | GateState::QueuedAfterWindow => {
                return Err(MacLinkReject::EventPending);
            }
        };
        self.0.set(queued);
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn release_failed_send(&self) {
        self.0.set(match self.0.get() {
            GateState::QueuedBeforeWindow => GateState::BeforeWindow,
            GateState::QueuedAfterWindow => GateState::Ready,
            state => state,
        });
    }

    pub(crate) fn event_deferred_to_startup(&self) {
        self.0.set(GateState::StartupIntentPending);
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn event_applied_to_window(&self) {
        self.0.set(GateState::Ready);
    }

    pub(crate) fn window_started(&self) {
        self.0.set(match self.0.get() {
            GateState::StartupIntentPending | GateState::BeforeWindow => GateState::Ready,
            GateState::QueuedBeforeWindow => GateState::QueuedAfterWindow,
            state => state,
        });
    }
}

/// Parse the Foundation URL string using the core's canonical parser.
pub(crate) fn parse_open_url(url: &str) -> Result<DeepLinkIntent, MacLinkReject> {
    DeepLinkIntent::parse(url).map_err(|_| MacLinkReject::InvalidUrl)
}

pub(crate) fn defer_startup_intent(
    pending: &mut Option<DeepLinkIntent>,
    intent: DeepLinkIntent,
    gate: &MacLinkGate,
) -> Result<(), MacLinkReject> {
    if pending.is_some() {
        gate.event_deferred_to_startup();
        return Err(MacLinkReject::StartupIntentPending);
    }
    *pending = Some(intent);
    gate.event_deferred_to_startup();
    Ok(())
}

fn check_url_count(count: usize) -> Result<(), MacLinkReject> {
    if count == 1 {
        Ok(())
    } else {
        Err(MacLinkReject::TooManyUrls)
    }
}

#[cfg(target_os = "macos")]
mod appkit {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2::{ClassType, DeclaredClass, declare_class, msg_send_id, mutability};
    use objc2_app_kit::{NSApplication, NSApplicationDelegate};
    use objc2_foundation::{MainThreadMarker, NSArray, NSObject, NSObjectProtocol, NSURL};
    use winit::event_loop::EventLoopProxy;

    use crate::{HostEvent, macos_links::MacLinkGate};

    use kattri_client_core::deep_link::MAX_DEEP_LINK_BYTES;

    use super::{MacLinkReject, check_url_count, parse_open_url};

    struct UrlDelegateIvars {
        proxy: EventLoopProxy<HostEvent>,
        gate: MacLinkGate,
    }

    declare_class!(
        struct UrlDelegate;

        unsafe impl ClassType for UrlDelegate {
            type Super = NSObject;
            type Mutability = mutability::MainThreadOnly;
            const NAME: &'static str = "KattriMacUrlDelegate";
        }

        impl DeclaredClass for UrlDelegate {
            type Ivars = UrlDelegateIvars;
        }

        unsafe impl NSObjectProtocol for UrlDelegate {}

        unsafe impl NSApplicationDelegate for UrlDelegate {
            #[method(application:openURLs:)]
            #[allow(non_snake_case)] // AppKit's delegate selector is camelCase by contract.
            fn application_openURLs(&self, _application: &NSApplication, urls: &NSArray<NSURL>) {
                // A panic must not unwind through AppKit's Objective-C callback boundary.
                let _ = catch_unwind(AssertUnwindSafe(|| {
                    if self.dispatch_urls(urls).is_err() {
                        eprintln!("rejected macOS Jellyfin URL event");
                    }
                }));
            }
        }
    );

    impl UrlDelegate {
        fn new(
            mtm: MainThreadMarker,
            proxy: EventLoopProxy<HostEvent>,
            gate: MacLinkGate,
        ) -> Retained<Self> {
            let ivars = UrlDelegateIvars { proxy, gate };
            // SAFETY: the delegate is created and retained on AppKit's main thread.
            unsafe { msg_send_id![super(mtm.alloc().set_ivars(ivars)), init] }
        }

        fn dispatch_urls(&self, urls: &NSArray<NSURL>) -> Result<(), MacLinkReject> {
            check_url_count(urls.count())?;

            // SAFETY: count was checked to be exactly one and AppKit supplies NSURL objects.
            let url = unsafe { urls.objectAtIndex(0) };
            // SAFETY: NSURL's absoluteString selector is available on the supported macOS floor.
            let absolute = unsafe { url.absoluteString() }.ok_or(MacLinkReject::InvalidUrl)?;
            if absolute.len_utf16() > MAX_DEEP_LINK_BYTES || absolute.len() > MAX_DEEP_LINK_BYTES {
                return Err(MacLinkReject::InvalidUrl);
            }
            let text = absolute.to_string();
            let intent = parse_open_url(&text)?;

            self.ivars().gate.reserve()?;
            if self
                .ivars()
                .proxy
                .send_event(HostEvent::MacOpenUrl(intent))
                .is_err()
            {
                self.ivars().gate.release_failed_send();
                return Err(MacLinkReject::EventLoopClosed);
            }
            Ok(())
        }
    }

    #[derive(Debug)]
    pub(crate) enum RegistrationError {
        NotMainThread,
        ExistingApplicationDelegate,
    }

    impl std::fmt::Display for RegistrationError {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::NotMainThread => formatter.write_str("AppKit main thread is unavailable"),
                Self::ExistingApplicationDelegate => formatter.write_str(
                    "an application delegate is already registered; preserving its lifecycle",
                ),
            }
        }
    }

    impl std::error::Error for RegistrationError {}

    pub(crate) struct Registration {
        _delegate: Retained<UrlDelegate>,
    }

    pub(crate) fn register(
        proxy: EventLoopProxy<HostEvent>,
        gate: MacLinkGate,
    ) -> Result<Registration, RegistrationError> {
        let mtm = MainThreadMarker::new().ok_or(RegistrationError::NotMainThread)?;
        // Winit's macOS contract requires this call after EventLoop::build.
        let application = NSApplication::sharedApplication(mtm);
        // The pinned platform guide permits a custom delegate, but winit 0.30.13's event-loop
        // source currently installs WinitApplicationDelegate. Preserve it; replacing it can break
        // winit's launch and teardown callbacks.
        // SAFETY: AppKit is being accessed on its main thread.
        if unsafe { application.delegate() }.is_some() {
            return Err(RegistrationError::ExistingApplicationDelegate);
        }

        let delegate = UrlDelegate::new(mtm, proxy, gate);
        application.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        Ok(Registration {
            _delegate: delegate,
        })
    }
}

#[cfg(target_os = "macos")]
pub(crate) use appkit::register;

#[cfg(test)]
mod tests {
    use super::{
        GateState, MacLinkGate, MacLinkReject, check_url_count, defer_startup_intent,
        parse_open_url,
    };

    const VALID_URL: &str = "jellyfin://kattri/v1/item/0123456789abcdef0123456789abcdef?server=https%3A%2F%2Fmedia.example%2Fjellyfin";

    #[test]
    fn accepts_a_valid_review_intent_without_interpreting_playback_actions() {
        let intent = parse_open_url(VALID_URL).expect("valid core deep link");
        assert_eq!(intent.item_id(), "0123456789abcdef0123456789abcdef");
        assert!(parse_open_url(&format!("{VALID_URL}&play=true")).is_err());
    }

    #[test]
    fn rejects_credential_bearing_urls() {
        let url = "jellyfin://kattri/v1/item/0123456789abcdef0123456789abcdef?server=https%3A%2F%2Fuser%3Asecret%40media.example";
        assert_eq!(parse_open_url(url), Err(MacLinkReject::InvalidUrl));
    }

    #[test]
    fn rejects_oversized_and_malformed_urls_without_echoing_input() {
        assert!(parse_open_url(&"x".repeat(8 * 1024 + 1)).is_err());
        assert_eq!(check_url_count(0), Err(MacLinkReject::TooManyUrls));
        assert_eq!(check_url_count(2), Err(MacLinkReject::TooManyUrls));
        assert_eq!(
            parse_open_url("jellyfin://kattri/v1/item/invalid"),
            Err(MacLinkReject::InvalidUrl)
        );
    }

    #[test]
    fn gate_allows_only_one_pre_window_event_until_startup_consumes_it() {
        let gate = MacLinkGate::new(false);
        gate.reserve().expect("first URL reserves the one slot");
        assert_eq!(gate.reserve(), Err(MacLinkReject::EventPending));
        let mut pending = None;
        defer_startup_intent(
            &mut pending,
            parse_open_url(VALID_URL).expect("valid core deep link"),
            &gate,
        )
        .expect("startup holds one validated intent");
        assert_eq!(
            pending.as_ref().map(|intent| intent.item_id()),
            Some("0123456789abcdef0123456789abcdef")
        );
        assert_eq!(gate.reserve(), Err(MacLinkReject::StartupIntentPending));
        gate.window_started();
        assert_eq!(gate.0.get(), GateState::Ready);
        gate.reserve().expect("window can accept a new URL");
    }

    #[test]
    fn a_command_line_intent_blocks_a_second_startup_url() {
        let gate = MacLinkGate::new(true);
        assert_eq!(gate.reserve(), Err(MacLinkReject::StartupIntentPending));
    }

    #[test]
    fn startup_routing_never_overwrites_an_existing_intent() {
        let gate = MacLinkGate::new(false);
        let original = parse_open_url(VALID_URL).expect("valid core deep link");
        let mut pending = Some(original.clone());
        let second = parse_open_url(
            "jellyfin://kattri/v1/item/abcdef0123456789abcdef0123456789?server=https%3A%2F%2Fmedia.example",
        )
        .expect("second valid core deep link");
        assert_eq!(
            defer_startup_intent(&mut pending, second, &gate),
            Err(MacLinkReject::StartupIntentPending)
        );
        assert_eq!(pending, Some(original));
    }
}

use std::{
    ffi::{CStr, c_char, c_void},
    marker::PhantomData,
    panic::{AssertUnwindSafe, catch_unwind},
    ptr,
    rc::Rc,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use crate::{
    DecodeInteropEvidence, DecodeInteropResult, MpvError, NativeDisplay, RenderContract,
    contract::BackendSelection,
    ffi::{
        MPV_RENDER_FRAME_INFO_BLOCK_VSYNC, MPV_RENDER_FRAME_INFO_PRESENT,
        MPV_RENDER_PARAM_API_TYPE, MPV_RENDER_PARAM_BLOCK_FOR_TARGET_TIME, MPV_RENDER_PARAM_FLIP_Y,
        MPV_RENDER_PARAM_INVALID, MPV_RENDER_PARAM_NEXT_FRAME_INFO, MPV_RENDER_PARAM_OPENGL_FBO,
        MPV_RENDER_PARAM_OPENGL_INIT_PARAMS, MPV_RENDER_PARAM_WL_DISPLAY,
        MPV_RENDER_PARAM_X11_DISPLAY, MpvOpenGlFbo, MpvOpenGlInitParams, MpvRenderContext,
        MpvRenderFrameInfo, MpvRenderParam,
    },
    session::MpvSession,
};

/// The host must make the same OpenGL context current for creation, every
/// render call, swap reporting, and destruction. Its native display handles
/// must outlive the libmpv render context. This is an unsafe trait because an
/// invalid function pointer or stale native handle can cause process-level UB.
///
/// An egui_glow integration must preserve egui's GL state in the returned
/// [`CurrentGlContext`] guard. A generic Egui `FallbackEgl` choice alone does
/// not satisfy the hardware-decode interop requirements in [`DecodeInteropEvidence`].
///
/// # Safety
///
/// Implementations must uphold the native-context invariants described above:
/// every guard must keep the exact compatible GL context current for its whole
/// lifetime on the context's owning thread; resolved function pointers and
/// native display handles must remain valid for all libmpv calls; and the
/// state-restore guard must restore all host GL state that the implementation
/// or egui relies on. Violating these requirements can cause undefined
/// behavior in libmpv or the host renderer.
pub unsafe trait GlContextProvider: Send + Sync + 'static {
    fn make_current(&self) -> Result<Box<dyn CurrentGlContext + '_>, String>;

    /// Resolve a GL symbol while `make_current`'s guard is alive.
    fn get_proc_address(&self, name: &CStr) -> *mut c_void;

    /// Return platform facts established by the host integration. These facts
    /// gate hardware-decode claims but do not configure `hwdec` by themselves.
    fn interop_evidence(&self) -> DecodeInteropEvidence;
}

/// A scope in which the exact GL context used at renderer creation is current.
pub trait CurrentGlContext {
    fn native_display(&self) -> Option<NativeDisplay> {
        None
    }

    /// Return a guard that restores all GL state relied on by egui_glow when
    /// dropped. Restore framebuffer bindings, viewport/scissor, program,
    /// textures, blend state, and any other state the host backend depends on.
    fn preserve_egui_state(&self) -> Box<dyn GlStateRestore + '_>;
}

/// Drop this guard to restore egui_glow's prior GL state.
pub trait GlStateRestore {}

pub trait RepaintNotifier: Send + Sync + 'static {
    fn request_repaint(&self);
}

impl<F> RepaintNotifier for F
where
    F: Fn() + Send + Sync + 'static,
{
    fn request_repaint(&self) {
        self()
    }
}

struct RepaintState {
    pending: AtomicBool,
    notifier: Arc<dyn RepaintNotifier>,
}

/// Coalesced update notification. The libmpv callback only sets this flag and
/// calls the host's thread-safe repaint notifier; it never calls libmpv.
#[derive(Clone)]
pub struct RepaintHandle(Arc<RepaintState>);

impl RepaintHandle {
    pub fn take_pending(&self) -> bool {
        self.0.pending.swap(false, Ordering::AcqRel)
    }
}

/// A checked OpenGL target. Dimensions are physical framebuffer pixels, not
/// egui logical points. The same dimensions are passed to libmpv on every draw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FboTarget {
    fbo: i32,
    width: i32,
    height: i32,
    internal_format: i32,
    flip_y: i32,
}

impl FboTarget {
    pub fn new(
        fbo: u32,
        width_pixels: u32,
        height_pixels: u32,
        internal_format: Option<u32>,
        flip_y: bool,
    ) -> Result<Self, MpvError> {
        if fbo > i32::MAX as u32
            || width_pixels == 0
            || height_pixels == 0
            || width_pixels > i32::MAX as u32
            || height_pixels > i32::MAX as u32
            || internal_format.is_some_and(|format| format > i32::MAX as u32)
        {
            return Err(MpvError::InvalidFramebuffer);
        }
        Ok(Self {
            fbo: fbo as i32,
            width: width_pixels as i32,
            height: height_pixels as i32,
            internal_format: internal_format.unwrap_or(0) as i32,
            flip_y: i32::from(flip_y),
        })
    }

    pub fn dimensions_pixels(self) -> (u32, u32) {
        (self.width as u32, self.height as u32)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderFrameInfo {
    pub target_time_us: i64,
    pub present: bool,
    pub block_vsync: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameDecision {
    NoFrame,
    WaitUntil {
        target_time_us: i64,
        delay: Duration,
    },
    Rendered(RenderFrameInfo),
}

/// One libmpv OpenGL render context. It is !Send and remains tied to its
/// session; all render entry points make the host context current and bracket
/// libmpv with the host's egui GL-state guard.
pub struct MpvRenderer<'session> {
    session: &'session MpvSession,
    context: *mut MpvRenderContext,
    provider: Arc<dyn GlContextProvider>,
    proc_context: Option<Box<ProcContext>>,
    callback_context: Option<Box<RenderCallbackContext>>,
    repaint: RepaintHandle,
    contract: RenderContract,
    _not_send: PhantomData<Rc<()>>,
}

impl<'session> MpvRenderer<'session> {
    pub fn create(
        session: &'session MpvSession,
        provider: Arc<dyn GlContextProvider>,
        notifier: Arc<dyn RepaintNotifier>,
        backend: BackendSelection,
    ) -> Result<Self, MpvError> {
        if backend != BackendSelection::OpenGlLibMpv {
            return Err(MpvError::UnsupportedBackend("gpu-next"));
        }
        let core = &session.core;
        core.render_attached
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| MpvError::RenderAlreadyAttached)?;

        let result = Self::create_claimed(session, provider, notifier);
        if result.is_err() {
            core.render_attached.store(false, Ordering::Release);
        }
        result
    }

    fn create_claimed(
        session: &'session MpvSession,
        provider: Arc<dyn GlContextProvider>,
        notifier: Arc<dyn RepaintNotifier>,
    ) -> Result<Self, MpvError> {
        let current = provider
            .make_current()
            .map_err(MpvError::GlContextUnavailable)?;
        let _state = current.preserve_egui_state();
        let native_display = current.native_display();

        let repaint = RepaintHandle(Arc::new(RepaintState {
            pending: AtomicBool::new(true),
            notifier,
        }));
        let mut proc_context = Box::new(ProcContext {
            provider: Arc::clone(&provider),
        });
        let mut callback_context = Box::new(RenderCallbackContext {
            gate: CallbackGate::default(),
            repaint: repaint.clone(),
        });

        let api_type = c"opengl";
        let mut gl_init = MpvOpenGlInitParams {
            get_proc_address: Some(get_proc_address),
            get_proc_address_ctx: (&mut *proc_context as *mut ProcContext).cast(),
        };
        let mut params = vec![
            MpvRenderParam {
                type_: MPV_RENDER_PARAM_API_TYPE,
                data: api_type.as_ptr().cast_mut().cast(),
            },
            MpvRenderParam {
                type_: MPV_RENDER_PARAM_OPENGL_INIT_PARAMS,
                data: (&mut gl_init as *mut MpvOpenGlInitParams).cast(),
            },
        ];
        if let Some(display) = native_display {
            params.push(MpvRenderParam {
                type_: match display {
                    NativeDisplay::X11(_) => MPV_RENDER_PARAM_X11_DISPLAY,
                    NativeDisplay::Wayland(_) => MPV_RENDER_PARAM_WL_DISPLAY,
                },
                data: display.raw(),
            });
        }
        params.push(MpvRenderParam {
            type_: MPV_RENDER_PARAM_INVALID,
            data: ptr::null_mut(),
        });

        let mut render_context = ptr::null_mut();
        // SAFETY: The session owns an initialized mpv core; the GL context is
        // current, the parameter storage remains alive for this call, and all
        // callback contexts are stable heap allocations retained by Self.
        let result = unsafe {
            (session.core.api.functions.render_context_create)(
                &mut render_context,
                session.core.handle(),
                params.as_mut_ptr(),
            )
        };
        if result < 0 || render_context.is_null() {
            return Err(MpvError::RenderContextCreate(result));
        }

        // SAFETY: callback_context remains pinned in its Box until it is
        // unregistered and the context has been freed in close().
        unsafe {
            (session
                .core
                .api
                .functions
                .render_context_set_update_callback)(
                render_context,
                Some(render_update),
                (&mut *callback_context as *mut RenderCallbackContext).cast(),
            );
        }
        drop(_state);
        drop(current);

        Ok(Self {
            session,
            context: render_context,
            provider,
            proc_context: Some(proc_context),
            callback_context: Some(callback_context),
            repaint,
            contract: RenderContract::opengl_candidate(),
            _not_send: PhantomData,
        })
    }

    pub fn contract(&self) -> RenderContract {
        self.contract
    }

    pub fn repaint_handle(&self) -> RepaintHandle {
        self.repaint.clone()
    }

    pub fn hardware_interop_assessment(&self) -> DecodeInteropResult {
        self.provider.interop_evidence().assess()
    }

    /// Render only when libmpv's next-frame target time is due. The host should
    /// schedule another repaint after the returned delay for `WaitUntil`.
    /// Rendering sets BLOCK_FOR_TARGET_TIME=0, so egui's paint callback cannot
    /// be held up by libmpv's A/V timing wait.
    pub fn render_if_due(&mut self, target: FboTarget) -> Result<FrameDecision, MpvError> {
        let current = self
            .provider
            .make_current()
            .map_err(MpvError::GlContextUnavailable)?;
        let _state = current.preserve_egui_state();

        // A callback during/after this clear raises the flag again and queues
        // another egui repaint, so an update is not lost while rendering.
        self.repaint.take_pending();
        let mut frame_info = MpvRenderFrameInfo {
            flags: 0,
            target_time: 0,
        };
        let info_param = MpvRenderParam {
            type_: MPV_RENDER_PARAM_NEXT_FRAME_INFO,
            data: (&mut frame_info as *mut MpvRenderFrameInfo).cast(),
        };
        // SAFETY: This render call uses the same current GL context used to
        // create the render context; frame_info is writable and correctly laid out.
        let info_result = unsafe {
            (self.session.core.api.functions.render_context_get_info)(self.context, info_param)
        };
        if info_result < 0 {
            return Err(MpvError::RenderCall(info_result));
        }
        if frame_info.flags & MPV_RENDER_FRAME_INFO_PRESENT == 0 {
            return Ok(FrameDecision::NoFrame);
        }

        // mpv_get_time_us is documented safe on render API threads. Compare in
        // libmpv's own clock domain before drawing.
        // SAFETY: The session handle remains valid, and upstream documents this
        // function as safe from a render API thread.
        let now =
            unsafe { (self.session.core.api.functions.get_time_us)(self.session.core.handle()) };
        if let Some(delay) = frame_wait(frame_info.target_time, now) {
            return Ok(FrameDecision::WaitUntil {
                target_time_us: frame_info.target_time,
                delay,
            });
        }

        let mut fbo = MpvOpenGlFbo {
            fbo: target.fbo,
            width: target.width,
            height: target.height,
            internal_format: target.internal_format,
        };
        let mut flip_y = target.flip_y;
        let mut block_for_target_time = 0;
        let mut params = [
            MpvRenderParam {
                type_: MPV_RENDER_PARAM_OPENGL_FBO,
                data: (&mut fbo as *mut MpvOpenGlFbo).cast(),
            },
            MpvRenderParam {
                type_: MPV_RENDER_PARAM_FLIP_Y,
                data: (&mut flip_y as *mut i32).cast(),
            },
            MpvRenderParam {
                type_: MPV_RENDER_PARAM_BLOCK_FOR_TARGET_TIME,
                data: (&mut block_for_target_time as *mut i32).cast(),
            },
            MpvRenderParam {
                type_: MPV_RENDER_PARAM_INVALID,
                data: ptr::null_mut(),
            },
        ];
        // SAFETY: The FBO is supplied by the current host GL context and its
        // physical dimensions are checked by FboTarget::new.
        let render_result = unsafe {
            (self.session.core.api.functions.render_context_render)(
                self.context,
                params.as_mut_ptr(),
            )
        };
        if render_result < 0 {
            return Err(MpvError::RenderCall(render_result));
        }

        Ok(FrameDecision::Rendered(RenderFrameInfo {
            target_time_us: frame_info.target_time,
            present: true,
            block_vsync: frame_info.flags & MPV_RENDER_FRAME_INFO_BLOCK_VSYNC != 0,
        }))
    }

    /// Call after the host actually presents/swaps the rendered frame. Use it
    /// consistently if the host can observe presentation timing; mpv documents
    /// inconsistent swap reporting as a source of timing errors.
    pub fn report_swap(&mut self) -> Result<(), MpvError> {
        let current = self
            .provider
            .make_current()
            .map_err(MpvError::GlContextUnavailable)?;
        let _state = current.preserve_egui_state();
        // SAFETY: The renderer's matching GL context is current.
        unsafe {
            (self.session.core.api.functions.render_context_report_swap)(self.context);
        }
        Ok(())
    }

    /// Explicitly free the renderer while its GL context can still be made
    /// current. If this returns an error, the renderer remains live and can be
    /// closed again later.
    pub fn close(&mut self) -> Result<(), MpvError> {
        if self.context.is_null() {
            return Ok(());
        }
        let current = self
            .provider
            .make_current()
            .map_err(MpvError::GlContextUnavailable)?;
        let _state = current.preserve_egui_state();
        let context = self.context;
        // First prevent any new repaint callbacks, then wait for callbacks
        // already running, then free the context while GL remains current.
        // SAFETY: context is live, callback storage is stable, and unregister
        // is permitted from this thread with the matching context current.
        unsafe {
            (self
                .session
                .core
                .api
                .functions
                .render_context_set_update_callback)(context, None, ptr::null_mut());
        }
        if let Some(callback) = self.callback_context.as_ref() {
            callback.gate.close_and_wait();
        }
        // SAFETY: The required matching GL context is current and all callbacks
        // have been unregistered and drained.
        unsafe {
            (self.session.core.api.functions.render_context_free)(context);
        }
        drop(_state);
        drop(current);
        self.context = ptr::null_mut();
        self.session
            .core
            .render_attached
            .store(false, Ordering::Release);
        self.callback_context.take();
        self.proc_context.take();
        Ok(())
    }
}

impl Drop for MpvRenderer<'_> {
    fn drop(&mut self) {
        if self.context.is_null() {
            return;
        }
        if self.close().is_ok() {
            return;
        }

        // A lost GL context makes render_context_free invalid. Unregister and
        // drain callbacks, then intentionally retain the render context/core
        // and callback resolver rather than call GL through a dead context or
        // destroy the mpv core while its render context still exists.
        // SAFETY: The update callback setter does not access GL state; the
        // context and callback storage remain alive until after this call.
        unsafe {
            (self
                .session
                .core
                .api
                .functions
                .render_context_set_update_callback)(
                self.context, None, ptr::null_mut()
            );
        }
        if let Some(callback) = self.callback_context.as_ref() {
            callback.gate.close_and_wait();
        }
        self.session
            .core
            .destruction_safe
            .store(false, Ordering::Release);
        if let Some(context) = self.callback_context.take() {
            let _ = Box::leak(context);
        }
        if let Some(context) = self.proc_context.take() {
            let _ = Box::leak(context);
        }
    }
}

struct ProcContext {
    provider: Arc<dyn GlContextProvider>,
}

struct RenderCallbackContext {
    gate: CallbackGate,
    repaint: RepaintHandle,
}

unsafe extern "C" fn get_proc_address(context: *mut c_void, name: *const c_char) -> *mut c_void {
    if context.is_null() || name.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: context points to the stable ProcContext box retained by the
    // renderer; libmpv supplies a NUL-terminated GL symbol name.
    let result = catch_unwind(AssertUnwindSafe(|| unsafe {
        let context = &*context.cast::<ProcContext>();
        context.provider.get_proc_address(CStr::from_ptr(name))
    }));
    result.unwrap_or(ptr::null_mut())
}

unsafe extern "C" fn render_update(context: *mut c_void) {
    if context.is_null() {
        return;
    }
    // SAFETY: context points to the stable callback box retained until the
    // callback is unregistered, drained, and the render context is freed.
    let _ = catch_unwind(AssertUnwindSafe(|| unsafe {
        let context = &*context.cast::<RenderCallbackContext>();
        let Some(_active) = context.gate.enter() else {
            return;
        };
        let repaint = &context.repaint.0;
        if !repaint.pending.swap(true, Ordering::AcqRel) {
            repaint.notifier.request_repaint();
        }
    }));
}

#[derive(Default)]
pub(crate) struct CallbackGate {
    state: Mutex<GateState>,
    idle: Condvar,
}

#[derive(Default)]
struct GateState {
    closing: bool,
    active: usize,
}

impl CallbackGate {
    pub(crate) fn enter(&self) -> Option<CallbackLease<'_>> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.closing {
            return None;
        }
        state.active += 1;
        Some(CallbackLease { gate: self })
    }

    pub(crate) fn close_and_wait(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.closing = true;
        while state.active != 0 {
            state = self
                .idle
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }
}

pub(crate) struct CallbackLease<'a> {
    gate: &'a CallbackGate,
}

impl Drop for CallbackLease<'_> {
    fn drop(&mut self) {
        let mut state = self
            .gate
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.active -= 1;
        if state.closing && state.active == 0 {
            self.gate.idle.notify_all();
        }
    }
}

pub(crate) fn wakeup_callback(gate: &CallbackGate, callback: impl FnOnce()) {
    let Some(_active) = gate.enter() else {
        return;
    };
    callback();
}

/// A frame is never really scheduled more than this far ahead.
const MAX_FRAME_WAIT_US: i64 = 2_000_000;

/// How long to wait before drawing a frame whose libmpv target time is `target`, when libmpv's
/// microsecond clock reads `now_us`. `None` means draw it now.
///
/// Older libmpv builds report the target time in microseconds; recent ones report it in
/// nanoseconds (the same base as `mpv_get_time_ns`). Reading a nanosecond target as
/// microseconds put every frame about a thousand times too far ahead, so the video never
/// drew. Both readings are tried, and a target that fits neither is treated as due.
fn frame_wait(target: i64, now_us: i64) -> Option<Duration> {
    [target, target / 1_000]
        .into_iter()
        .find_map(|candidate_us| {
            let ahead = candidate_us.checked_sub(now_us)?;
            (ahead > 0 && ahead <= MAX_FRAME_WAIT_US).then(|| Duration::from_micros(ahead as u64))
        })
}

#[cfg(test)]
mod frame_wait_tests {
    use super::frame_wait;
    use std::time::Duration;

    #[test]
    fn due_and_late_frames_draw_now() {
        assert_eq!(frame_wait(1_000, 1_000), None);
        assert_eq!(frame_wait(500, 1_000), None);
    }

    #[test]
    fn a_microsecond_target_a_few_milliseconds_ahead_waits() {
        assert_eq!(
            frame_wait(1_016_000, 1_000_000),
            Some(Duration::from_millis(16))
        );
    }

    #[test]
    fn a_nanosecond_target_is_read_as_nanoseconds() {
        // 31.793357501 s in ns against a clock reading 31.531 s in us: 262.357 ms ahead.
        assert_eq!(
            frame_wait(31_793_357_501, 31_531_000),
            Some(Duration::from_micros(262_357))
        );
        // The same reading, already past: draw now.
        assert_eq!(frame_wait(31_793_357_501, 31_900_000), None);
    }

    #[test]
    fn a_target_that_fits_neither_unit_draws_now() {
        assert_eq!(frame_wait(900_000_000_000_000, 1_000_000), None);
    }

    #[test]
    fn extreme_values_never_overflow_or_wait() {
        for (target, now) in [
            (i64::MIN, 0),
            (i64::MAX, 0),
            (0, i64::MAX),
            (0, i64::MIN),
            (i64::MAX, i64::MIN),
            (i64::MIN, i64::MAX),
            (-1, 0),
        ] {
            assert_eq!(frame_wait(target, now), None, "{target} vs {now}");
        }
    }

    #[test]
    fn a_zero_or_missing_target_draws_now() {
        assert_eq!(frame_wait(0, 0), None);
        assert_eq!(frame_wait(0, 5_000_000), None);
    }

    #[test]
    fn the_wait_limit_is_inclusive_in_microseconds() {
        let limit = super::MAX_FRAME_WAIT_US;
        let now = 10_000_000;
        assert_eq!(
            frame_wait(now + limit, now),
            Some(Duration::from_micros(limit as u64))
        );
        // One microsecond further is not a plausible frame time in either unit.
        assert_ne!(
            frame_wait(now + limit + 1, now),
            Some(Duration::from_micros(limit as u64 + 1))
        );
    }

    #[test]
    fn real_nanosecond_targets_at_24_and_60_fps_spacing_wait_about_one_frame() {
        // mpv_get_time_ns-based targets one frame ahead of a microsecond clock.
        let now_us = 120_000_000;
        let now_ns = now_us * 1_000;
        for frame_us in [41_708_i64, 16_667] {
            let wait = frame_wait(now_ns + frame_us * 1_000, now_us);
            assert_eq!(wait, Some(Duration::from_micros(frame_us as u64)));
        }
    }

    #[test]
    fn microsecond_and_nanosecond_readings_of_the_same_instant_agree() {
        let now_us = 31_531_000;
        let ahead_us = 262_357_i64;
        let as_us = frame_wait(now_us + ahead_us, now_us);
        let as_ns = frame_wait((now_us + ahead_us) * 1_000, now_us);
        assert_eq!(as_us, as_ns);
        assert_eq!(as_us, Some(Duration::from_micros(ahead_us as u64)));
    }
}

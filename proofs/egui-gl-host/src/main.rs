//! Custom native host that embeds KattriApp and presents libmpv into egui.

// Release builds are GUI apps: without this, Windows treats the exe as a console program and
// opens a terminal window beside the player when it is launched from the Start Menu. Debug builds
// keep the console so development output stays visible.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(debug_assertions)]
mod debug_harness;
#[cfg(debug_assertions)]
mod position_trace;
#[cfg(debug_assertions)]
mod qa_metrics;
use std::error::Error;
use std::ffi::CString;
#[cfg(any(debug_assertions, test))]
use std::ffi::OsStr;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::ViewportId;
use egui_winit::State as EguiWinitState;
use gl_adapter::{ContextParts, HostGlContextProvider};
use glow::HasContext;
use glutin::config::{Config, ConfigTemplate, ConfigTemplateBuilder};
use glutin::context::{ContextApi, ContextAttributesBuilder, NotCurrentGlContext};
use glutin::display::{Display, DisplayApiPreference, GetGlDisplay, GlDisplay};
use glutin::prelude::GlSurface;
use glutin::surface::{SwapInterval, WindowSurface};
use jellyfin_egui_client::BurnInReplacementEnd;
use jellyfin_egui_client::{KattriApp, RuntimeAbout};
use kattri_client_core::PlaybackPhase;
use kattri_client_core::controls::{OsdVisibility, PlaybackSpeed};
use kattri_client_core::deep_link::DeepLinkIntent;
use kattri_client_core::pip::{PipController, PipError, PipSize, PipTransition, can_show_video};
use kattri_client_core::screenshot::{
    ScreenshotContext, ScreenshotFormat, screenshot_stem, unique_file_name,
};
use kattri_client_core::window_state::SavedWindow;
use kattri_platform::{PowerGuard, PowerInhibitor, PowerStatus, default_power_inhibitor};
use kattri_plugins::discord::{IpcTransport, PresenceBinding};
use mpv_host::{
    ItemLoadSettings, MpvHost, RelayLoadOptions, TrackMemoryEvent, TrackMenuKind,
    VideoFrameSizeRequest, local_media_path,
};
use mpv_render_proof::{
    CalibrationMarker, FboTarget, FrameDecision, MpvLibrary, MpvSession,
    PROVISIONAL_SCREENSHOT_MODE,
};
use std::path::{Path, PathBuf};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalSize};
use winit::event::{StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use winit::window::{Window, WindowAttributes, WindowId};

mod blur;
mod debug_validation;
mod display_switch;
mod gl_adapter;
#[cfg(windows)]
mod global_shortcuts;
mod launch_intent;
#[cfg(any(target_os = "macos", test))]
mod macos_links;
mod media_info_panel;
mod mpv_host;
mod player_controls;
use launch_intent::parse_host_args;
mod player_icons;
mod report_handoff;
mod syncplay_bridge;
mod video_target;
mod window_hooks;

use player_controls::{
    ChapterChoice, GamepadPlayerAction, PlayerControlAction, PlayerControlsView,
    SecondarySubtitleChoice, SkipControls, screenshot_key_pressed,
};
use report_handoff::{CloseAdmission, ReportCloseGrace};
use video_target::VideoTarget;

/// How long a status-bar notice (screenshot or PiP) stays on screen.
const SHELL_NOTICE_DURATION: Duration = Duration::from_secs(3);
/// The pointer must be still this long in fullscreen before its cursor is hidden.
const CURSOR_HIDE_AFTER: Duration = Duration::from_secs(2);

pub(crate) enum HostEvent {
    EguiRepaint(Duration),
    MpvRepaint,
    AccessKit(egui_winit::accesskit_winit::Event),
    #[cfg(windows)]
    GlobalShortcut(global_shortcuts::GlobalAction),
    #[cfg(target_os = "macos")]
    MacOpenUrl(DeepLinkIntent),
}

impl From<egui_winit::accesskit_winit::Event> for HostEvent {
    fn from(event: egui_winit::accesskit_winit::Event) -> Self {
        Self::AccessKit(event)
    }
}

struct GlHost<'session> {
    mpv: MpvHost<'session>,
    video_target: Option<VideoTarget>,
    /// A requested video-FBO size waiting for the window to stop moving, so a fullscreen toggle
    /// does not reallocate and clear the texture on every transition frame.
    pending_video_target: Option<([u32; 2], Instant)>,
    painter: Option<egui_glow::Painter>,
    egui_winit: EguiWinitState,
    kattri_app: KattriApp,
    egui: egui::Context,
    gl: Arc<glow::Context>,
    max_texture_size: u32,
    parts: Rc<ContextParts>,
    // The context and surface retain the native window handle, so the window
    // is declared last and explicitly kept alive through GL teardown.
    window: Window,
    video_texture_id: egui::TextureId,
    blur_target: Option<blur::VideoBlurTarget>,
    blur_texture_id: Option<egui::TextureId>,
    blur_unavailable: bool,
    blur_dirty: bool,
    screen_framebuffer: i32,
    physical_size: PhysicalSize<u32>,
    shutdown_error: Option<String>,
    report_close_grace: ReportCloseGrace,
    report_connection_generation: u64,
    report_connection_notice: Option<String>,
    sync_play_bridge: syncplay_bridge::SyncPlaybackBridge,
    syncplay_status_opt_in: bool,
    osd_visibility: OsdVisibility,
    osd_clock: Instant,
    /// Fade of the player chrome, driven by the OSD timer but stepped smoothly so the row and the
    /// top overlay appear and disappear together.
    chrome_opacity: f32,
    destroyed: bool,
    player_input_active: bool,
    focus_before_player: Option<egui::Id>,
    // The screenshot command goes to the same session the MpvHost renders from.
    session: Option<&'session MpvSession>,
    pip: PipController,
    // Name of a local media file, used for the `%f` template field. Jellyfin items use their id.
    local_stem: Option<String>,
    screenshot_count: u32,
    shell_notice: Option<(String, Duration)>,
    /// Keeps the OS from sleeping or turning the display off while media is loaded.
    power: Box<dyn PowerInhibitor>,
    power_guard: Option<PowerGuard>,
    power_error_logged: bool,
    power_retry_at: Option<Instant>,
    display_switch: display_switch::DisplaySwitch,
    // Pending window-geometry write. Moves and resizes restart it; it is polled from about_to_wait.
    geometry_save: window_hooks::SaveDebounce,
    // Requested state is cached to avoid sending redundant window-level hints every frame.
    window_level_topmost: bool,
    /// Last time pointer or keyboard activity arrived, for the fullscreen cursor timer.
    cursor_idle_since: Instant,
    cursor_hidden: bool,
    /// Winit focus state. A defocused window always shows its cursor again.
    window_focused: bool,
    #[cfg(windows)]
    defocus_minimizer: window_hooks::DefocusMinimizer,
}

/// Report a recoverable failure through the log file, and to stderr in debug builds.
/// The release binary is a Windows GUI-subsystem process with no console, so stderr is lost there.
fn report_warning(message: impl std::fmt::Display) {
    tracing::warn!("{message}");
    #[cfg(debug_assertions)]
    eprintln!("{message}");
}

/// Report a failure that stops the host or loses teardown work, keeping the text in the log file.
/// The release binary is a Windows GUI-subsystem process with no console, so stderr is lost there.
fn report_error(message: impl std::fmt::Display) {
    tracing::error!("{message}");
    #[cfg(debug_assertions)]
    eprintln!("{message}");
}

/// Window and taskbar icon: the Jellyfin mark in the Kattri red gradient, generated by
/// `packaging/icons/build_icons.py`. Raw RGBA keeps the host free of an image decoder.
fn window_icon() -> Option<winit::window::Icon> {
    const SIZE: u32 = 128;
    const RGBA: &[u8] = include_bytes!("../../../packaging/icons/kattri-128.rgba");
    winit::window::Icon::from_rgba(RGBA.to_vec(), SIZE, SIZE).ok()
}

/// Open the GL display and pick the window config, or report why that failed.
///
/// `glutin_winit::DisplayBuilder::build` keeps its picker on the `-> Config` signature, so an
/// empty config list cannot become an error there. This repeats the same bootstrap (same
/// preferences, same call order) to return a typed error instead of panicking.
fn create_window_and_config(
    event_loop: &ActiveEventLoop,
    window_attributes: WindowAttributes,
    template: ConfigTemplateBuilder,
) -> Result<(Option<Window>, Config), Box<dyn Error>> {
    #[cfg(windows)]
    {
        // The WGL backend needs the native window before it can pick a config.
        let window = event_loop.create_window(window_attributes)?;
        let raw_window = window.window_handle()?.as_raw();
        // SAFETY: the raw handles come from the live window and event loop, which outlive the
        // display.
        let display = unsafe {
            Display::new(
                event_loop.display_handle()?.as_raw(),
                DisplayApiPreference::WglThenEgl(Some(raw_window)),
            )?
        };
        let template = template.compatible_with_native_window(raw_window).build();
        let config = pick_config(&display, template)?;
        Ok((Some(window), config))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // SAFETY: the display handle comes from the live event loop.
        let display = unsafe {
            Display::new(
                event_loop.display_handle()?.as_raw(),
                DisplayApiPreference::GlxThenEgl(Box::new(
                    winit::platform::x11::register_xlib_error_hook,
                )),
            )?
        };
        let config = pick_config(&display, template.build())?;
        let window = glutin_winit::finalize_window(event_loop, window_attributes, &config)?;
        Ok((Some(window), config))
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: the display handle comes from the live event loop.
        let display = unsafe {
            Display::new(
                event_loop.display_handle()?.as_raw(),
                DisplayApiPreference::Cgl,
            )?
        };
        let config = pick_config(&display, template.build())?;
        let window = glutin_winit::finalize_window(event_loop, window_attributes, &config)?;
        Ok((Some(window), config))
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = (event_loop, window_attributes, template);
        Err(std::io::Error::other("no GL display backend for this platform").into())
    }
}

/// First config the display offers; the window and context calls must use that display.
fn pick_config(display: &Display, template: ConfigTemplate) -> Result<Config, Box<dyn Error>> {
    // SAFETY: any raw handles in the template come from the live host window.
    let mut configs = unsafe { display.find_configs(template)? };
    let Some(config) = configs.next() else {
        return Err(
            std::io::Error::other("the platform returned no compatible GL configurations").into(),
        );
    };
    Ok(config)
}

impl<'session> GlHost<'session> {
    fn new(
        event_loop: &ActiveEventLoop,
        proxy: EventLoopProxy<HostEvent>,
        session: Option<&'session MpvSession>,
        runtime_error: Option<String>,
        local_media: Result<Option<String>, String>,
        profile: Option<&str>,
        launch_intent: Option<DeepLinkIntent>,
    ) -> Result<Self, Box<dyn Error>> {
        let window_attributes = WindowAttributes::default()
            .with_title("Jellyfin Kattri")
            .with_inner_size(LogicalSize::new(1180.0, 760.0))
            .with_window_icon(window_icon())
            .with_visible(false);
        let template = ConfigTemplateBuilder::new()
            .with_depth_size(0)
            .with_stencil_size(0)
            .with_transparency(false);

        let (window, config) =
            create_window_and_config(event_loop, window_attributes.clone(), template)?;
        let display = config.display();
        let window = match window {
            Some(window) => window,
            None => glutin_winit::finalize_window(event_loop, window_attributes, &config)?,
        };

        let raw_window = window.window_handle()?.as_raw();
        let context_attributes = ContextAttributesBuilder::new().build(Some(raw_window));
        let gles_attributes = ContextAttributesBuilder::new()
            .with_context_api(ContextApi::Gles(None))
            .build(Some(raw_window));

        // SAFETY: the raw window handle is obtained from the live `window`,
        // which is retained for at least as long as the GL context and surface.
        let not_current = unsafe {
            display
                .create_context(&config, &context_attributes)
                .or_else(|_| display.create_context(&config, &gles_attributes))?
        };

        let physical_size = window.inner_size();
        let (width, height) = nonzero_size(physical_size);
        let surface_attributes = glutin::surface::SurfaceAttributesBuilder::<WindowSurface>::new()
            .build(raw_window, width, height);

        // SAFETY: the raw window handle remains valid because `window` is
        // stored in this host for the entire lifetime of the surface.
        let surface = unsafe { display.create_window_surface(&config, &surface_attributes)? };
        let context = not_current.make_current(&surface)?;
        let parts = Rc::new(ContextParts {
            context,
            surface,
            display,
        });
        if let Err(error) = parts
            .surface
            .set_swap_interval(&parts.context, SwapInterval::Wait(NonZeroU32::MIN))
        {
            report_warning(format!("could not enable swap interval: {error}"));
        }

        let gl = Arc::new(unsafe {
            // SAFETY: the matching context is current and the loader is backed
            // by the display retained in ContextParts.
            glow::Context::from_loader_function(|name| {
                // A name with an interior NUL cannot be a GL symbol; report it as missing.
                let Ok(name) = CString::new(name) else {
                    return std::ptr::null();
                };
                parts.display.get_proc_address(&name)
            })
        });
        let mut painter = egui_glow::Painter::new(Arc::clone(&gl), "", None, false)?;
        let screen_framebuffer = unsafe { gl.get_parameter_i32(glow::FRAMEBUFFER_BINDING) };
        let egui = egui::Context::default();
        let repaint_proxy = proxy.clone();
        egui.set_request_repaint_callback(move |info| {
            let _ = repaint_proxy.send_event(HostEvent::EguiRepaint(info.delay));
        });
        let mut egui_winit = EguiWinitState::new(
            egui.clone(),
            ViewportId::ROOT,
            &window,
            Some(window.scale_factor() as f32),
            window.theme(),
            None,
        );
        egui_winit.set_max_texture_side(unsafe {
            gl.get_parameter_i32(glow::MAX_TEXTURE_SIZE).max(1) as usize
        });
        egui_winit.init_accesskit(event_loop, &window, proxy.clone());

        let local_stem = local_media
            .as_ref()
            .ok()
            .and_then(|path| path.as_deref())
            .and_then(|path| Path::new(path).file_stem())
            .map(|stem| stem.to_string_lossy().into_owned());
        let syncplay_status_opt_in = debug_syncplay_status_opt_in();
        let video_target = VideoTarget::new(&gl, [640, 360])?;
        let video_texture_id = painter.register_native_texture(video_target.texture());
        let blur_target = None;
        let blur_texture_id = None;
        let blur_unavailable = false;
        let provider = HostGlContextProvider::new(Rc::clone(&parts), Arc::clone(&gl));
        let max_texture_size = provider.capabilities().max_texture_size;
        let gpu_identity = unsafe {
            format!(
                "{} / {}",
                gl.get_parameter_string(glow::VENDOR),
                gl.get_parameter_string(glow::RENDERER)
            )
        };
        let driver_identity = unsafe { gl.get_parameter_string(glow::VERSION) };
        let mut mpv = MpvHost::new(
            session,
            runtime_error,
            provider,
            proxy,
            gpu_identity,
            driver_identity,
        );
        #[cfg(debug_assertions)]
        if stream_only_validation(std::env::var_os("KATTRI_DEBUG_STREAM_ONLY").as_deref()) {
            mpv.disable_reporting_for_validation()?;
        }
        // Same launch rules as the egui client: `--profile` picks the profile, a missing one
        // falls back with a notice, and the picker opens when several profiles exist.
        // The app draws the notice and the picker itself from `ui`.
        let mut kattri_app = KattriApp::with_launch_profile(egui.clone(), profile);
        let resume_saved_sign_in =
            should_resume_saved_sign_in(&local_media, launch_intent.is_some());
        if let Some(intent) = launch_intent {
            // An external link uses the selected/active profile and requires explicit sign-in.
            kattri_app.apply_launch_intent(intent);
        } else {
            if resume_saved_sign_in {
                kattri_app.resume_saved_sign_in();
            }
        }
        // Load the local file after the app exists, so its hwdec comes from the launch profile.
        mpv.load_local_media(
            local_media,
            kattri_app.video_settings(),
            kattri_app.subtitle_settings(),
        );
        // The window is still hidden, so the restored placement is not seen as a jump (M2-14).
        let saved_window = window_hooks::saved_from_settings(kattri_app.window_settings());
        window_hooks::restore_at_startup(&window, event_loop, saved_window.as_ref());
        let window_level_topmost = kattri_app.window_settings().always_on_top;
        window_hooks::apply_window_level(&window, window_level_topmost);
        // The restore may change the inner size after the surface was created. Resize it to match.
        let physical_size = window.inner_size();
        let (width, height) = nonzero_size(physical_size);
        parts.surface.resize(&parts.context, width, height);
        let report_connection_generation = kattri_app.connection_generation();
        let osd_clock = Instant::now();
        let mut osd_visibility = OsdVisibility::new();
        osd_visibility.on_input(Duration::ZERO);
        window.set_visible(true);

        Ok(Self {
            mpv,
            video_target: Some(video_target),
            pending_video_target: None,
            painter: Some(painter),
            egui_winit,
            kattri_app,
            egui,
            gl,
            max_texture_size,
            parts,
            window,
            video_texture_id,
            blur_target,
            blur_texture_id,
            blur_unavailable,
            blur_dirty: true,
            screen_framebuffer,
            physical_size,
            shutdown_error: None,
            report_close_grace: ReportCloseGrace::default(),
            report_connection_generation,
            report_connection_notice: None,
            sync_play_bridge: syncplay_bridge::SyncPlaybackBridge::default(),
            syncplay_status_opt_in,
            osd_visibility,
            osd_clock,
            chrome_opacity: 0.0,
            destroyed: false,
            player_input_active: false,
            focus_before_player: None,
            session,
            pip: PipController::new(),
            local_stem,
            screenshot_count: 0,
            shell_notice: None,
            power: default_power_inhibitor(),
            power_guard: None,
            power_error_logged: false,
            power_retry_at: None,
            display_switch: display_switch::DisplaySwitch::new(),
            geometry_save: window_hooks::SaveDebounce::new(window_hooks::SAVE_DEBOUNCE),
            window_level_topmost,
            cursor_idle_since: osd_clock,
            cursor_hidden: false,
            window_focused: true,
            #[cfg(windows)]
            defocus_minimizer: window_hooks::DefocusMinimizer::default(),
        })
    }

    fn touch_window_geometry(&mut self) {
        self.geometry_save.touch(Instant::now());
    }

    fn window_geometry_deadline(&self) -> Option<Instant> {
        self.geometry_save.deadline()
    }

    /// Save the window now, unless PiP is on: the PiP size is not the window memory (M2-14).
    /// The write is queued to the settings writer thread, so this does not wait for the disk.
    fn save_window_geometry(&mut self, event_loop: &ActiveEventLoop) {
        if self.pip.is_pip() {
            return;
        }
        // A window that reports no position or monitor keeps the last saved geometry.
        if let Some(normal) = window_hooks::capture_window(&self.window, event_loop) {
            self.store_normal_window(&normal);
        }
    }

    fn store_normal_window(&mut self, normal: &SavedWindow) {
        if window_hooks::is_storable(normal) {
            self.kattri_app.save_window_geometry(normal);
        }
    }

    fn save_window_geometry_if_due(&mut self, event_loop: &ActiveEventLoop, now: Instant) {
        if self.geometry_save.take_due(now) {
            self.save_window_geometry(event_loop);
        }
    }

    fn sync_window_level(&mut self) {
        let requested = window_hooks::should_be_always_on_top(
            self.kattri_app.window_settings().always_on_top,
            self.pip.is_pip(),
        );
        if requested != self.window_level_topmost {
            window_hooks::apply_window_level(&self.window, requested);
            self.window_level_topmost = requested;
        }
    }

    fn apply_pip_transition(&mut self, transition: &PipTransition) {
        self.window_level_topmost = window_hooks::apply_transition(
            &self.window,
            transition,
            self.kattri_app.window_settings().always_on_top,
        );
    }

    #[cfg(windows)]
    fn on_window_focus_changed(&mut self, focused: bool, now: Instant) {
        self.defocus_minimizer.focus_changed(
            focused,
            self.kattri_app.window_settings().minimize_on_defocus,
            now,
        );
    }

    #[cfg(windows)]
    fn minimize_on_defocus_if_due(&mut self, now: Instant) {
        let enabled = self.kattri_app.window_settings().minimize_on_defocus;
        let app_window_is_foreground = window_hooks::app_process_has_foreground_window();
        if self
            .defocus_minimizer
            .take_due(now, enabled, app_window_is_foreground)
            && self.window.is_minimized() != Some(true)
        {
            self.window.set_minimized(true);
        }
    }

    fn resize(&mut self, size: PhysicalSize<u32>) {
        self.physical_size = size;
        if let (Some(width), Some(height)) =
            (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
        {
            self.parts
                .surface
                .resize(&self.parts.context, width, height);
        }
    }

    fn show_cursor(&mut self) {
        if self.cursor_hidden {
            self.window.set_cursor_visible(true);
            self.cursor_hidden = false;
        }
    }

    /// Hide the pointer in fullscreen after it has been still, and show it again as soon as the
    /// window is not a fullscreen, focused, idle surface. The player keeps receiving input either
    /// way; only the pointer's visibility changes.
    fn update_cursor_visibility(&mut self, fullscreen: bool, input_seen: bool) {
        let now = Instant::now();
        if input_seen {
            self.cursor_idle_since = now;
        }
        let idle = now.saturating_duration_since(self.cursor_idle_since);
        if cursor_should_hide(fullscreen, self.window_focused, idle, input_seen) {
            if !self.cursor_hidden {
                self.window.set_cursor_visible(false);
                self.cursor_hidden = true;
            }
        } else {
            self.show_cursor();
        }
    }

    /// When the fullscreen cursor is due to hide, so the event loop can wake for it.
    fn cursor_hide_deadline(&self) -> Option<Instant> {
        if self.cursor_hidden || !self.window_focused || self.window.fullscreen().is_none() {
            return None;
        }
        self.cursor_idle_since.checked_add(CURSOR_HIDE_AFTER)
    }

    fn render(&mut self, event_loop: &ActiveEventLoop) -> Duration {
        #[cfg(debug_assertions)]
        let qa_frame_started = Instant::now();
        self.sync_report_connection();
        self.mpv
            .set_sync_play_speed_locked(self.kattri_app.sync_play_speed_locked());
        self.mpv.poll_events();
        self.apply_pending_video_frame_size();
        self.poll_sync_play_status();
        #[cfg(debug_assertions)]
        debug_harness::log_if_changed("mpv status", self.mpv.status());
        // Leaving PiP when the video stops is decided by the model, once per frame (M2-12).
        if self.pip.is_pip() {
            let (phase, has_video) = self.pip_playback();
            if !can_show_video(phase, has_video) {
                let monitors = window_hooks::monitor_infos(event_loop);
                if let Some(transition) = self.pip.playback_changed(phase, has_video, &monitors) {
                    self.apply_pip_transition(&transition);
                }
            }
        }
        if let Some(generation) = self.mpv.take_playback_loaded() {
            self.kattri_app.playback_loaded(generation);
        }
        let playback_end = self.mpv.take_playback_end();
        #[cfg(debug_assertions)]
        if playback_end
            .as_ref()
            .is_some_and(|end| end.reason == mpv_render_proof::EndFileReason::Error)
        {
            position_trace::note_player_error();
        }
        let replacement_end = playback_end
            .as_ref()
            .map(|end| {
                self.kattri_app.on_playback_end(
                    end.generation,
                    end.was_loaded,
                    end.reason == mpv_render_proof::EndFileReason::Error,
                    end.command_submission_failed,
                )
            })
            .unwrap_or(BurnInReplacementEnd::None);
        if let Some(mpv_host::PlaybackEnd {
            reason,
            item_id: Some(item_id),
            was_live,
            generation,
            was_loaded,
            command_submission_failed: _,
            position_ticks,
        }) = playback_end
        {
            if !matches!(
                replacement_end,
                BurnInReplacementEnd::Suppress | BurnInReplacementEnd::Recovering
            ) {
                self.sync_play_bridge.end(generation);
            }
            #[cfg(debug_assertions)]
            debug_harness::log_if_changed(
                "mpv end",
                &format!("{reason:?} item={item_id} live={was_live}"),
            );
            if matches!(
                replacement_end,
                BurnInReplacementEnd::Suppress
                    | BurnInReplacementEnd::Recovering
                    | BurnInReplacementEnd::Failed
            ) {
                // The subtitle replacement owns this end event and its recovery.
            } else if was_live {
                match reason {
                    mpv_render_proof::EndFileReason::Eof
                    | mpv_render_proof::EndFileReason::Error => {
                        self.kattri_app.on_live_tv_stream_ended(&item_id);
                    }
                    mpv_render_proof::EndFileReason::Stop
                    | mpv_render_proof::EndFileReason::Quit
                    | mpv_render_proof::EndFileReason::Redirect
                    | mpv_render_proof::EndFileReason::Unknown(_) => {}
                }
            } else {
                match reason {
                    mpv_render_proof::EndFileReason::Eof => {
                        if self.kattri_app.music_queue_current_item_id() == Some(item_id.as_str()) {
                            self.kattri_app.on_music_track_ended(&item_id, false);
                        } else {
                            self.kattri_app.on_episode_ended(&item_id);
                        }
                    }
                    mpv_render_proof::EndFileReason::Error => {
                        if self.kattri_app.music_queue_current_item_id() == Some(item_id.as_str()) {
                            self.kattri_app.playback_failed(&item_id);
                        } else {
                            match (was_loaded, generation) {
                                (false, Some(generation)) => {
                                    self.kattri_app.playback_open_failed(&item_id, generation)
                                }
                                (true, Some(generation)) => {
                                    self.kattri_app.playback_failed_mid_stream(
                                        &item_id,
                                        generation,
                                        position_ticks,
                                    )
                                }
                                _ if generation.is_none_or(|id| {
                                    id == self.kattri_app.playback_start_generation()
                                }) =>
                                {
                                    self.kattri_app.playback_failed(&item_id)
                                }
                                _ => {}
                            }
                        }
                    }
                    mpv_render_proof::EndFileReason::Stop
                    | mpv_render_proof::EndFileReason::Quit
                    | mpv_render_proof::EndFileReason::Redirect
                    | mpv_render_proof::EndFileReason::Unknown(_) => {}
                }
            }
        }
        #[cfg(debug_assertions)]
        {
            // One per-client sample four times a second while a file is open, for the SyncPlay
            // drift analyzer. `is_playing` is the loaded-file flag, not the paused state.
            let controls = self.mpv.controls_snapshot();
            position_trace::observe(
                self.mpv.is_playing(),
                controls.position_seconds,
                controls.paused,
                self.mpv.sync_play_sample().cache_paused,
                Instant::now(),
            );
        }
        if let Some(profile) = self.mpv.take_device_profile() {
            self.kattri_app.set_device_profile(profile);
        }
        apply_log_detail(self.kattri_app.log_detail());
        if self
            .mpv
            .sync_audio_filters(self.kattri_app.audio_settings(), Instant::now())
        {
            self.egui
                .request_repaint_after(mpv_host::AUDIO_FILTER_APPLY_GAP);
        }
        self.mpv
            .sync_subtitle_style(self.kattri_app.subtitle_settings());
        if let Some(devices) = self.mpv.take_audio_devices() {
            self.kattri_app.set_audio_devices(devices);
        }
        self.update_display_switch();
        self.flush_playback_reports();
        if self.physical_size.width == 0 || self.physical_size.height == 0 {
            return self.report_retry_delay().unwrap_or(Duration::MAX);
        }
        // `mut` is only used by the debug harness injection below.
        #[allow(unused_mut)]
        let mut input = self.egui_winit.take_egui_input(&self.window);
        #[cfg(debug_assertions)]
        debug_harness::inject(&mut input, &self.window, &self.egui);
        // The picker closes this window once the replacement process has started. egui-winit
        // reports that request as a viewport input event, not as a window event.
        if input
            .viewport()
            .events
            .iter()
            .any(|event| matches!(event, egui::ViewportEvent::Close))
        {
            self.begin_close();
            if self.advance_close() {
                event_loop.exit();
            } else {
                self.window.request_redraw();
            }
            return Duration::MAX;
        }
        let renderer_available = self.mpv.has_renderer();
        let media_active = self.mpv.has_media();
        let mpv_loading = self.mpv.is_loading();
        let playback_preparing = self.kattri_app.playback_preparing();
        if self.pip.is_pip() && !media_active && !playback_preparing && !mpv_loading {
            self.toggle_pip(event_loop);
        }
        let pip_active = self.pip.is_pip();
        let player_surface_active =
            player_surface_active(media_active, playback_preparing, mpv_loading, pip_active);
        let theater = should_use_theater_view(renderer_available, player_surface_active);
        if player_surface_active && !self.player_input_active {
            self.focus_before_player = self.egui.memory(|memory| memory.focused());
            if let Some(focused) = self.focus_before_player {
                self.egui
                    .memory_mut(|memory| memory.surrender_focus(focused));
            }
        } else if !player_surface_active
            && self.player_input_active
            && let Some(focused) = self.focus_before_player.take()
        {
            self.egui.memory_mut(|memory| memory.request_focus(focused));
        }
        self.player_input_active = player_surface_active;
        self.kattri_app.set_player_owns_input(player_surface_active);
        let mut gamepad_actions = if player_surface_active {
            match self.kattri_app.poll_player_gamepad_actions() {
                Ok(actions) => actions,
                Err(error) => {
                    tracing::warn!(%error, "Could not poll the player gamepad");
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };
        gamepad_actions.extend(self.kattri_app.take_player_nav_actions());
        if !gamepad_actions.is_empty() {
            self.osd_visibility.on_input(self.osd_clock.elapsed());
        }
        let player_had_focus = self.egui.memory(|memory| memory.focused().is_some());
        let user_activity = has_user_activity(&input.events);
        if user_activity {
            self.osd_visibility.on_input(self.osd_clock.elapsed());
        }
        // Playback shortcuts must not escape an egui popup or a text field, even if the popup does
        // not currently request text input (for example, a ComboBox menu). Any other focused
        // control, such as a button in the player details panel, keeps them working.
        let typing = self.egui.text_edit_focused();
        let popup_open = egui::Popup::is_any_open(&self.egui);
        let shortcuts_enabled = playback_shortcuts_enabled(typing, popup_open);
        if shortcuts_enabled {
            for action in mpv_host::playback_key_actions(&input.events) {
                // The aspect cycle needs the settings store to persist the new mode, which lives
                // on the app side; every other playback key is handled by the player alone.
                if matches!(action, mpv_host::PlaybackKeyAction::CycleAspect) {
                    self.cycle_aspect();
                } else {
                    self.mpv.dispatch_playback_key(action);
                }
            }
        }
        let mut fullscreen_toggle_requested = false;
        if !self.egui.egui_wants_keyboard_input()
            && let Some(action) = host_shortcut_action(&input.events)
        {
            match action {
                HostShortcutAction::ToggleFullscreen => fullscreen_toggle_requested = true,
                HostShortcutAction::Minimize => self.window.set_minimized(true),
                HostShortcutAction::ToggleAlwaysOnTop => {
                    self.kattri_app.toggle_always_on_top();
                    let enabled = self.kattri_app.window_settings().always_on_top;
                    self.sync_window_level();
                    self.set_shell_notice(if enabled {
                        "Always on top: On".to_owned()
                    } else {
                        "Always on top: Off".to_owned()
                    });
                }
                HostShortcutAction::ToggleTvMode => {
                    let enabled = self.kattri_app.toggle_tv_mode();
                    self.set_shell_notice(if enabled {
                        "TV Mode: On".to_owned()
                    } else {
                        "TV Mode: Off".to_owned()
                    });
                }
            }
        }
        let screenshot_requested = shortcuts_enabled && screenshot_key_pressed(&input.events);
        let player_shortcut = shortcuts_enabled
            .then(|| player_controls::player_shortcut_action(&input.events))
            .flatten();
        let mut video_size_points = egui::vec2(640.0, 360.0);
        let video_texture_id = self.video_texture_id;
        let blur_texture_id = self.blur_texture_id;
        let playback_status = self.mpv.status().to_owned();
        let capability_warning = self
            .mpv
            .capability_warning()
            .map(|warning| format!("Runtime capability fallback: {warning:?}"));
        let shutdown_error = self.shutdown_error.clone();
        let report_handoff_feedback = self
            .mpv
            .report_handoff_feedback()
            .or_else(|| self.report_connection_notice.clone());
        let track_feedback = self.mpv.track_feedback().map(str::to_owned);
        let controls_snapshot = self.mpv.controls_snapshot();
        let playing_item_id = self.mpv.playing_item_id().map(str::to_owned);
        self.kattri_app.observe_active_player_position(
            playing_item_id.as_deref(),
            controls_snapshot.position_seconds,
            controls_snapshot.paused,
            controls_snapshot.speed_multiplier,
        );
        self.kattri_app.observe_music_queue_position(
            playing_item_id.as_deref(),
            controls_snapshot.position_seconds,
        );
        let controls_position = controls_snapshot
            .position_seconds
            .and_then(duration_from_seconds)
            .unwrap_or_default();
        let controls_duration = controls_snapshot
            .duration_seconds
            .and_then(duration_from_seconds);
        let controls_paused = controls_snapshot.paused;
        let controls_speed_multiplier = controls_snapshot.speed_multiplier;
        let (previous_track_enabled, next_track_enabled) =
            self.mpv
                .playing_item_id()
                .map_or((false, false), |playing_id| {
                    if self.kattri_app.music_queue_current_item_id() == Some(playing_id) {
                        (true, true)
                    } else {
                        self.kattri_app.episode_navigation(playing_id)
                    }
                });
        self.osd_visibility
            .set_paused(controls_paused.unwrap_or(false));
        let osd_now = self.osd_clock.elapsed();
        if theater_skeleton_visible(playback_preparing, mpv_loading) {
            self.osd_visibility.on_input(osd_now);
        }
        let local_stream_only_media = self.local_stem.is_some() && !self.mpv.reporting_enabled();
        let controls_visible = player_controls_visible(
            self.osd_visibility.is_visible_at(osd_now) || local_stream_only_media,
            self.egui.memory(|memory| memory.focused().is_some()),
            egui::Popup::is_any_open(&self.egui) || self.mpv.media_info_open(),
        );
        // The chrome follows the existing OSD timer, but its opacity is stepped smoothly so the
        // bottom row and the top overlay appear and disappear as one surface. Reduced motion
        // (a zero style animation time) makes this snap.
        let chrome_target = if controls_visible { 1.0 } else { 0.0 };
        let fade = player_controls::fade_seconds(self.egui.global_style().animation_time);
        let frame_time = self.egui.input(|input| input.stable_dt);
        self.chrome_opacity = player_controls::advance_fade(
            self.chrome_opacity,
            chrome_target,
            Duration::from_secs_f32(frame_time.max(0.0)),
            Duration::from_secs_f32(fade),
        );
        let chrome_opacity = self.chrome_opacity;
        let chrome_present = chrome_opacity > 0.001;
        let osd_hide_after = self.osd_visibility.time_until_hidden(osd_now);
        let controls_feedback = self.mpv.controls_feedback().cloned();
        self.kattri_app.set_media_playing(media_active);
        let audio_track_options = chrome_present
            .then(|| self.mpv.track_menu_options(TrackMenuKind::Audio))
            .flatten();
        let subtitle_track_options = chrome_present
            .then(|| self.mpv.track_menu_options(TrackMenuKind::Subtitle))
            .flatten();
        let controls_chapters = if chrome_present {
            controls_snapshot
                .chapters
                .iter()
                .map(|chapter| ChapterChoice {
                    index: chapter.index,
                    title: chapter.title.clone(),
                    time: chapter.time,
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let burn_in_subtitle_tracks = chrome_present
            .then(|| {
                self.kattri_app.burn_in_subtitle_choices().map(|choices| {
                    choices
                        .into_iter()
                        .map(|(value, label, selected)| player_controls::TrackChoice {
                            value,
                            label,
                            selected,
                        })
                        .collect::<Vec<_>>()
                })
            })
            .flatten();
        let burn_in_subtitle_feedback = self
            .kattri_app
            .burn_in_subtitle_feedback()
            .map(str::to_owned);
        let playback_report_feedback = self
            .kattri_app
            .playback_report_feedback()
            .map(str::to_owned);
        let runtime_diagnostics = media_info_panel::RuntimeDiagnostics {
            status: &playback_status,
            capability_warning: capability_warning.as_deref(),
            report_feedback: playback_report_feedback.as_deref(),
        };
        // Off when no skip switch is on or the outro delay is invalid (M2-08).
        let skip_settings =
            player_controls::skip_settings(self.kattri_app.playback_settings()).ok();
        let media_info = self.mpv.media_info_view();
        let secondary_choices = if chrome_present {
            self.mpv.secondary_subtitle_choices()
        } else {
            Vec::new()
        };
        let secondary_selected = self.mpv.secondary_subtitle_selected();
        let playing_title = playing_item_id
            .as_deref()
            .and_then(|id| self.kattri_app.item_title(id))
            .map(str::to_owned)
            .or_else(|| self.local_stem.clone());
        // The app exposes only an item title publicly, so the second title line uses the chapter
        // that contains the playhead, which is the useful context a viewer has here.
        let playing_subtitle = if chrome_present {
            controls_chapters
                .iter()
                .rev()
                .find(|chapter| chapter.time <= controls_position)
                .map(|chapter| {
                    if chapter.title.trim().is_empty() {
                        format!("Chapter {}", chapter.index.saturating_add(1))
                    } else {
                        chapter.title.clone()
                    }
                })
        } else {
            None
        };
        let volume_percent = controls_snapshot.volume_percent;
        let volume_max_percent = self.kattri_app.audio_settings().max_volume_percent;
        let muted = controls_snapshot.muted;
        let fullscreen = self.window.fullscreen().is_some();
        self.update_cursor_visibility(fullscreen, user_activity);
        // In fullscreen there is no scrollable page behind the video, so the wheel seeks.
        if fullscreen
            && shortcuts_enabled
            && let Some(seconds) = mpv_host::wheel_seek_seconds(&input.events)
        {
            self.mpv.seek_relative(seconds);
        }
        if screenshot_requested {
            self.take_screenshot();
        }
        if let Some(action) = player_shortcut {
            self.apply_player_shortcut(action);
        }
        let pip_available = renderer_available && media_active;
        let mut pip_toggle_requested = false;
        let mut up_next_visible = false;
        let shell_notice = self.shell_notice.as_ref().and_then(|(text, shown_at)| {
            let age = osd_now.saturating_sub(*shown_at);
            (age < SHELL_NOTICE_DURATION).then(|| (text.clone(), SHELL_NOTICE_DURATION - age))
        });
        // The OSD fades out during playback, and the controls are not drawn then. Automatic
        // skip still has to run, so it is checked here as well (M2-08).
        if renderer_available
            && media_active
            && !controls_visible
            && let Some(settings) = skip_settings
            && let Some(PlayerControlAction::SeekAbsolute(target)) =
                player_controls::auto_skip_action(
                    SkipControls {
                        skipper: self.mpv.segment_skipper_mut(),
                        settings,
                    },
                    controls_position,
                    controls_duration,
                )
        {
            self.mpv.seek_absolute(target);
        }
        let osd = OsdState {
            position: controls_position,
            duration: controls_duration,
            paused: controls_paused,
            speed_multiplier: controls_speed_multiplier,
            media_active,
            previous_track_enabled,
            next_track_enabled,
            chapters: &controls_chapters,
            secondary_subtitles: &secondary_choices,
            secondary_selected,
            pip_active,
            pip_available,
            title: playing_title.as_deref(),
            subtitle: playing_subtitle.as_deref(),
            volume_percent,
            volume_max_percent,
            muted,
            fullscreen,
            opacity: chrome_opacity,
            skip_settings,
            feedback: controls_feedback.as_ref(),
            audio_tracks: audio_track_options.as_deref(),
            subtitle_tracks: subtitle_track_options.as_deref(),
            burn_in_subtitle_tracks: burn_in_subtitle_tracks.as_deref(),
            burn_in_subtitle_feedback: burn_in_subtitle_feedback.as_deref(),
            track_feedback: track_feedback.as_deref(),
        };
        // Active media uses the theater surface. A compact Media info action opens diagnostics
        // over the video instead of reserving a permanent side page.
        let activate_without_focus =
            append_gamepad_navigation_events(&mut input, &gamepad_actions, player_had_focus);
        let mpv = &mut self.mpv;
        let kattri_app = &mut self.kattri_app;
        let mut output = self.egui.run_ui(input, |root| {
            if media_active && kattri_app.take_player_pause_shortcut(root.ctx()) {
                if mpv.live_stream_active() {
                    mpv.stop_playback();
                } else {
                    mpv.toggle_paused();
                }
            }
            #[cfg(debug_assertions)]
            if !mpv.reporting_enabled() {
                let chrome = jellyfin_egui_client::active_palette();
                egui::Panel::top("stream_only_validation_notice")
                    .frame(
                        egui::Frame::new()
                            .fill(chrome.panel)
                            .inner_margin(egui::Margin::symmetric(16, 2)),
                    )
                    .show(root, |ui| {
                        ui.label(
                            egui::RichText::new(
                                "Stream-only validation — session reporting is disabled.",
                            )
                            .color(chrome.muted),
                        );
                    });
            }
            if let Some(delay) = osd_hide_after {
                root.request_repaint_after(delay);
            }
            if (chrome_opacity - chrome_target).abs() > 0.001 {
                // Step the fade while it is in flight; when it lands the regular OSD schedule
                // takes over again.
                root.request_repaint();
            }
            if media_info.is_some() {
                // The panel re-reads the player's properties on this cadence.
                root.request_repaint_after(media_info_panel::REFRESH_INTERVAL);
            }
            if let Some((_, remaining)) = &shell_notice {
                root.request_repaint_after(*remaining);
            }
            if theater {
                // Service the worker without laying out a hidden catalog, changing its focus,
                // or running catalog actions against player input.
                kattri_app.update_background(root.ctx());
                kattri_app.show_subtitle_search(root.ctx());
                let typing = root.egui_wants_keyboard_input();
                let escape_pressed = root.input(|input| input.key_pressed(egui::Key::Escape));
                match theater_escape_action(
                    root.ctx(),
                    escape_pressed,
                    typing,
                    kattri_app.subtitle_search_open(),
                    mpv.media_info_open(),
                ) {
                    TheaterEscapeAction::ClosePopup => {}
                    TheaterEscapeAction::CloseSubtitleSearch => {
                        kattri_app.dismiss_subtitle_search();
                    }
                    TheaterEscapeAction::CloseMediaInfo => mpv.close_media_info(),
                    TheaterEscapeAction::StopPlayback => {
                        if pip_active {
                            pip_toggle_requested = true;
                        } else {
                            mpv.stop_playback();
                            if playback_preparing {
                                kattri_app.cancel_playback_preparation();
                            }
                        }
                    }
                    TheaterEscapeAction::None => {}
                }
                if theater_skeleton_visible(playback_preparing, mpv_loading) {
                    root.request_repaint();
                    let skeleton_title = kattri_app.preparing_item_title().map(str::to_owned);
                    egui::CentralPanel::default()
                        .frame(egui::Frame::new().fill(egui::Color32::BLACK))
                        .show(root, |ui| {
                            let stage = ui.max_rect();
                            video_size_points = fit_16_9(stage.size());
                            let screen = ui.ctx().content_rect();
                            if !pip_active {
                                player_controls::paint_top_scrim(ui.painter(), stage, 1.0);
                                player_controls::paint_bottom_scrim(ui.painter(), stage, 1.0);
                                egui::Area::new(egui::Id::unique("theater_skeleton_top"))
                                    .order(egui::Order::Foreground)
                                    .fixed_pos(stage.left_top())
                                    .show(ui.ctx(), |ui| {
                                        ui.set_width(stage.width());
                                        let top_outcome = player_controls::draw_top_chrome_outcome(
                                            ui,
                                            player_controls::TopChromeView {
                                                title: Some(
                                                    skeleton_title.as_deref().unwrap_or("Loading"),
                                                ),
                                                subtitle: None,
                                                opacity: 1.0,
                                                media_info_open: false,
                                                show_media_info: false,
                                            },
                                        );
                                        if top_outcome.action
                                            == player_controls::TopChromeAction::Back
                                        {
                                            mpv.stop_playback();
                                            kattri_app.cancel_playback_preparation();
                                        }
                                    });
                            }
                            egui::Area::new(egui::Id::unique("theater_skeleton_spinner"))
                                .order(egui::Order::Foreground)
                                .anchor(
                                    egui::Align2::CENTER_CENTER,
                                    stage.center() - screen.center(),
                                )
                                .show(ui.ctx(), |ui| {
                                    ui.add(
                                        egui::Spinner::new()
                                            .size(36.0)
                                            .color(egui::Color32::from_white_alpha(200)),
                                    );
                                });
                            if pip_active {
                                let pip_hovered = ui.rect_contains_pointer(stage);
                                if pip_hovered || chrome_opacity > 0.01 {
                                    let pip_opacity =
                                        if pip_hovered { 1.0 } else { chrome_opacity };
                                    if player_controls::draw_pip_overlay(
                                        ui.ctx(),
                                        stage,
                                        osd.paused,
                                        pip_opacity,
                                    ) == player_controls::PipOverlayAction::ExitPip
                                    {
                                        pip_toggle_requested = true;
                                    }
                                }
                            } else {
                                draw_skeleton_bottom_bar(ui.ctx(), stage, screen);
                            }
                        });
                    return;
                }
                egui::CentralPanel::default()
                    .frame(egui::Frame::new().fill(egui::Color32::BLACK))
                    .show(root, |ui| {
                        let area = ui.max_rect();
                        let size = fit_16_9(area.size());
                        video_size_points = size;
                        let video_rect = egui::Rect::from_center_size(area.center(), size);
                        ui.painter().image(
                            video_texture_id,
                            video_rect,
                            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                            egui::Color32::WHITE,
                        );
                        let video_surface = ui.interact(
                            area,
                            ui.make_persistent_id("kattri-theater-video-surface"),
                            egui::Sense::CLICK,
                        );
                        label_video_surface(&video_surface, osd.title);
                        if pip_active {
                            if video_surface.clicked_by(egui::PointerButton::Primary) {
                                if mpv.live_stream_active() {
                                    mpv.stop_playback();
                                } else {
                                    mpv.toggle_paused();
                                }
                            }
                            let pip_hovered = ui.rect_contains_pointer(area);
                            if pip_hovered || chrome_opacity > 0.01 {
                                let pip_opacity = if pip_hovered { 1.0 } else { chrome_opacity };
                                match player_controls::draw_pip_overlay(
                                    ui.ctx(),
                                    area,
                                    osd.paused,
                                    pip_opacity,
                                ) {
                                    player_controls::PipOverlayAction::ExitPip => {
                                        pip_toggle_requested = true;
                                    }
                                    player_controls::PipOverlayAction::TogglePaused => {
                                        if mpv.live_stream_active() {
                                            mpv.stop_playback();
                                        } else {
                                            mpv.toggle_paused();
                                        }
                                    }
                                    player_controls::PipOverlayAction::None => {}
                                }
                            }
                            return;
                        }
                        let center_play_clicked = if chrome_present {
                            player_controls::draw_center_play(
                                ui.ctx(),
                                area,
                                osd.paused,
                                osd.media_active,
                                chrome_opacity,
                            )
                        } else {
                            false
                        };
                        let video_clicked = video_click_toggles(
                            &video_surface,
                            center_play_clicked,
                            egui::Popup::is_any_open(ui.ctx()),
                            mpv.media_info_open(),
                            kattri_app.subtitle_search_open(),
                        ) && mpv.has_video_track() == Some(true);
                        if video_clicked || center_play_clicked {
                            if mpv.live_stream_active() {
                                mpv.stop_playback();
                            } else {
                                mpv.toggle_paused();
                            }
                        }
                        let backdrop_blur = blur_texture_id.map(|texture_id| blur::BackdropBlur {
                            texture_id,
                            stage_rect: video_rect,
                        });
                        let mut top_media_info_hot = false;
                        if chrome_present {
                            // Keep the chrome inside the video area, not the whole window: a
                            // debug-only notice or any future top panel must not be covered.
                            let screen = ui.ctx().content_rect();
                            let stage = area;
                            player_controls::paint_top_scrim(ui.painter(), stage, chrome_opacity);
                            player_controls::paint_bottom_scrim(
                                ui.painter(),
                                stage,
                                chrome_opacity,
                            );
                            egui::Area::new(egui::Id::unique("theater_top"))
                                .order(egui::Order::Foreground)
                                .fixed_pos(stage.left_top())
                                .show(ui.ctx(), |ui| {
                                    ui.set_width(stage.width());
                                    let top_outcome = player_controls::draw_top_chrome_outcome(
                                        ui,
                                        player_controls::TopChromeView {
                                            title: osd.title,
                                            subtitle: osd.subtitle,
                                            opacity: chrome_opacity,
                                            media_info_open: mpv.media_info_open(),
                                            show_media_info: true,
                                        },
                                    );
                                    top_media_info_hot = top_outcome.media_info_hot;
                                    match top_outcome.action {
                                        player_controls::TopChromeAction::Back => {
                                            mpv.stop_playback();
                                            if playback_preparing {
                                                kattri_app.cancel_playback_preparation();
                                            }
                                        }
                                        player_controls::TopChromeAction::MediaInfo => {
                                            mpv.dispatch_playback_key(
                                                mpv_host::PlaybackKeyAction::MediaInfo,
                                            );
                                        }
                                        player_controls::TopChromeAction::None => {}
                                    }
                                });
                            egui::Area::new(egui::Id::unique("theater_controls"))
                                .order(egui::Order::Foreground)
                                .anchor(
                                    egui::Align2::LEFT_BOTTOM,
                                    egui::vec2(
                                        stage.left() - screen.left(),
                                        stage.bottom() - screen.bottom(),
                                    ),
                                )
                                .show(ui.ctx(), |ui| {
                                    ui.set_width(stage.width());
                                    osd_controls(
                                        ui,
                                        mpv,
                                        kattri_app,
                                        &osd,
                                        &mut pip_toggle_requested,
                                        &mut fullscreen_toggle_requested,
                                        backdrop_blur,
                                    );
                                });
                        }
                        up_next_visible =
                            up_next_card(ui.ctx(), mpv, kattri_app, &osd, backdrop_blur);
                        let was_media_info_open = mpv.media_info_open();
                        if let Some(info) = &media_info {
                            let outcome = media_info_panel::show(
                                ui.ctx(),
                                info,
                                runtime_diagnostics,
                                backdrop_blur,
                            );
                            if outcome.close {
                                mpv.close_media_info();
                            } else {
                                mpv.set_media_info_hover(
                                    top_media_info_hot || outcome.hovered_or_focused,
                                );
                            }
                        } else {
                            mpv.set_media_info_hover(top_media_info_hot);
                        }
                        if was_media_info_open != mpv.media_info_open() {
                            ui.ctx().request_repaint();
                        }
                    });
                return;
            }
            if report_handoff_feedback.is_some() || shutdown_error.is_some() {
                egui::Panel::top("native_playback_report_feedback").show(root, |ui| {
                    if let Some(feedback) = &report_handoff_feedback {
                        ui.colored_label(egui::Color32::LIGHT_RED, feedback);
                    }
                    if let Some(error) = &shutdown_error {
                        ui.colored_label(egui::Color32::LIGHT_RED, error);
                    }
                });
            }
            // The status bar appears only when it has something to say: media is loaded, the
            // player runtime is missing, or a notice is showing.
            if status_bar_visible(media_active, renderer_available, shell_notice.is_some()) {
                let chrome = jellyfin_egui_client::active_palette();
                egui::Panel::bottom("player_status_bar")
                    .exact_size(34.0)
                    .frame(
                        egui::Frame::new()
                            .fill(chrome.panel)
                            .inner_margin(egui::Margin::symmetric(16, 0)),
                    )
                    .show(root, |ui| {
                        ui.horizontal_centered(|ui| {
                            if !renderer_available {
                                let (dot, _) = ui.allocate_exact_size(
                                    egui::vec2(8.0, 8.0),
                                    egui::Sense::hover(),
                                );
                                ui.painter()
                                    .circle_filled(dot.center(), 4.0, chrome.warning);
                                ui.label(
                                    egui::RichText::new(
                                        "Video playback is unavailable on this computer",
                                    )
                                    .color(chrome.muted),
                                );
                            }
                            if let Some((text, _)) = &shell_notice {
                                ui.label(text);
                            }
                        });
                    });
            }
            // No frame margin: the app draws its own edge-to-edge bars and page gutters.
            egui::CentralPanel::default()
                .frame(egui::Frame::new())
                .show(root, |ui| kattri_app.ui(ui));
            let was_media_info_open = mpv.media_info_open();
            if let Some(info) = &media_info {
                let outcome = media_info_panel::show(root, info, runtime_diagnostics, None);
                if outcome.close {
                    mpv.close_media_info();
                } else {
                    mpv.set_media_info_hover(outcome.hovered_or_focused);
                }
            } else {
                mpv.set_media_info_hover(false);
            }
            if was_media_info_open != mpv.media_info_open() {
                root.request_repaint();
            }
        });
        if let Some(generation) = self.kattri_app.take_terminal_burn_in_replacement_failure() {
            self.sync_play_bridge.end(Some(generation));
        }
        self.apply_gamepad_player_actions(&gamepad_actions, activate_without_focus);
        self.sync_report_connection();
        self.flush_playback_reports();
        #[cfg(debug_assertions)]
        debug_harness::observe_tree(&output.platform_output);
        self.egui_winit.handle_platform_output_with_event_loop(
            &self.window,
            event_loop,
            output.platform_output,
        );
        // The window is changed after the UI pass, so no borrow of the UI is held here.
        if pip_toggle_requested {
            self.toggle_pip(event_loop);
        }
        if fullscreen_toggle_requested {
            if self.pip.is_pip() {
                let _ = self.pip.set_fullscreen_requested(true);
                self.toggle_pip(event_loop);
            } else {
                // Borderless fullscreen on the current monitor, the same mode the saved
                // placement restores, so leaving fullscreen returns to the stored window rect.
                let next = self
                    .window
                    .fullscreen()
                    .is_none()
                    .then_some(winit::window::Fullscreen::Borderless(None));
                self.window.set_fullscreen(next);
                self.cursor_idle_since = Instant::now();
                self.show_cursor();
                self.window.request_redraw();
            }
        }
        self.sync_window_level();
        // Track choices the viewer made this frame go to the per-series memory (M2-10).
        for (series_id, event) in self.mpv.take_track_memory_events() {
            let series = series_id.as_deref();
            match event {
                TrackMemoryEvent::Audio(track) => {
                    self.kattri_app.record_audio_choice(series, &track);
                }
                TrackMemoryEvent::Subtitle(track) => {
                    self.kattri_app.record_subtitle_choice(series, &track);
                }
                TrackMemoryEvent::SubtitlesOff => {
                    self.kattri_app.record_subtitles_off_choice(series);
                }
            }
        }
        // Remote-control actions from the egui client (M4-13). Drained here, on the UI
        // thread and before the frame renders, so libmpv only changes on this thread.
        for action in self.kattri_app.take_player_intents() {
            self.mpv.apply_remote_action(action);
        }
        if let Some(playback) = self.kattri_app.take_prepared_playback() {
            let playback_generation = self.kattri_app.playback_start_generation();
            let subtitle_replacement = playback.is_subtitle_replacement();
            let preserve_paused = playback.preserve_paused();
            self.sync_play_bridge.capture(
                playback_generation,
                self.kattri_app
                    .sync_play_playback_target(playback_generation),
            );
            #[cfg(debug_assertions)]
            qa_metrics::playback_queued(Instant::now());
            let series_id = playback.series_id().map(str::to_owned);
            let replacement_subtitle_stream_index = subtitle_replacement
                .then(|| playback.subtitle_stream_index())
                .flatten();
            let (url, relay, report_metadata, resume_position_ticks) = playback.into_parts();
            let live_stream_id = report_metadata.live_stream_id.clone();
            let live_channel_id = live_stream_id
                .as_ref()
                .map(|_| report_metadata.item_id.clone());
            let audio_settings = self.kattri_app.playback_audio_settings();
            let load = ItemLoadSettings {
                audio: &audio_settings,
                playback: self.kattri_app.playback_settings(),
                subtitles: self.kattri_app.subtitle_settings(),
                video: self.kattri_app.video_settings(),
                series_id,
                language_memory: self.kattri_app.series_language_memory(),
            };
            match self.mpv.load_relay(
                url,
                relay,
                report_metadata,
                resume_position_ticks,
                RelayLoadOptions {
                    generation: self.kattri_app.playback_start_generation(),
                    preserve_paused,
                },
                load,
            ) {
                Ok(()) => {
                    if subtitle_replacement {
                        self.kattri_app.playback_subtitle_replacement_opening(
                            playback_generation,
                            replacement_subtitle_stream_index,
                        );
                    }
                    // Catalog focus must not consume player shortcuts or keep its overlay open.
                    self.egui.memory_mut(|memory| {
                        if let Some(focus) = memory.focused() {
                            memory.surrender_focus(focus);
                        }
                    });
                    self.report_connection_notice = None;
                    self.kattri_app.playback_start_opening();
                }
                Err(error) => {
                    if !subtitle_replacement {
                        self.sync_play_bridge.end(Some(playback_generation));
                    }
                    if let Some(live_stream_id) = live_stream_id {
                        self.kattri_app.close_live_tv_stream(live_stream_id);
                    }
                    if let Some(channel_id) = live_channel_id {
                        self.kattri_app.on_live_tv_stream_ended(&channel_id);
                    }
                    if subtitle_replacement {
                        if self
                            .kattri_app
                            .playback_subtitle_replacement_failed(playback_generation)
                        {
                            self.sync_play_bridge.end(Some(playback_generation));
                        }
                    } else {
                        match error {
                            mpv_host::RelayLoadError::Busy => self.kattri_app.playback_start_busy(),
                            mpv_host::RelayLoadError::Failed(message) => {
                                self.kattri_app.playback_start_failed(message)
                            }
                        }
                    }
                }
            }
            self.egui.request_repaint();
        }

        // A fast segment response can arrive in the same frame as a prepared
        // item; stage that item first so its segments have a matching owner.
        if let Some((item_id, segments)) = self.kattri_app.take_media_segments() {
            self.mpv.load_media_segments(&item_id, segments);
        }

        // A subtitle downloaded from the player's search joins the video it was found for.
        for (item_id, subtitle) in self.kattri_app.take_player_subtitles() {
            if self.mpv.playing_item_id() == Some(item_id.as_str()) {
                self.mpv.add_external_subtitle(
                    &subtitle.path,
                    subtitle.title.as_deref(),
                    subtitle.language.as_deref(),
                );
            }
        }

        if let Some(stream_url) = self.kattri_app.take_stream_url() {
            // A direct stream address has no item, so no per-title subtitle style applies.
            self.kattri_app.on_playback_stopped();
            let audio_settings = self.kattri_app.playback_audio_settings();
            let load = ItemLoadSettings {
                audio: &audio_settings,
                playback: self.kattri_app.playback_settings(),
                subtitles: self.kattri_app.subtitle_settings(),
                video: self.kattri_app.video_settings(),
                series_id: None,
                language_memory: self.kattri_app.series_language_memory(),
            };
            if let Err(error) = self.mpv.load_stream_url(&stream_url, load) {
                tracing::warn!(error, "stream address was not started");
            }
            self.egui.request_repaint();
        }

        let target_size = physical_video_size(
            video_size_points,
            output.pixels_per_point,
            self.max_texture_size,
        );
        if let Err(error) = self.apply_video_target_size(target_size) {
            report_warning(format!("could not resize the video FBO: {error}"));
        }
        let decision = if let Some(target) = self.video_target.as_ref() {
            match FboTarget::new(
                target.framebuffer_id(),
                target.size()[0],
                target.size()[1],
                Some(target.internal_format()),
                // The video FBO texture is drawn with egui's top-left UV origin, and a GL
                // texture's first row is its bottom row, so mpv must not flip again.
                false,
            ) {
                Ok(fbo) => match self.mpv.render_if_due(fbo) {
                    Ok(decision) => {
                        if matches!(decision, FrameDecision::Rendered(_)) {
                            self.mpv.mark_frame_rendered();
                        }
                        decision
                    }
                    Err(error) => {
                        report_warning(format!("libmpv OpenGL render failed: {error}"));
                        FrameDecision::NoFrame
                    }
                },
                Err(error) => {
                    report_warning(format!("invalid libmpv FBO target: {error}"));
                    FrameDecision::NoFrame
                }
            }
        } else {
            FrameDecision::NoFrame
        };
        let blur_needed = theater
            && !pip_active
            && !theater_skeleton_visible(playback_preparing, mpv_loading)
            && (media_info.is_some()
                || up_next_visible
                || (chrome_present && egui::Popup::is_any_open(&self.egui)));
        if blur_needed {
            if self.blur_target.is_none()
                && !self.blur_unavailable
                && let Some(target) = self.video_target.as_ref()
            {
                match blur::VideoBlurTarget::new(Arc::clone(&self.gl), target.size()) {
                    Ok(mut blur) => {
                        blur.detach_output_texture();
                        if let Some(painter) = self.painter.as_mut() {
                            self.blur_texture_id =
                                Some(painter.register_native_texture(blur.output_texture()));
                        }
                        self.blur_target = Some(blur);
                        self.blur_dirty = true;
                    }
                    Err(error) => {
                        self.blur_unavailable = true;
                        report_warning(format!("video backdrop blur unavailable: {error}"));
                    }
                }
            }
            if (matches!(decision, FrameDecision::Rendered(_)) || self.blur_dirty)
                && let (Some(target), Some(blur)) =
                    (self.video_target.as_ref(), self.blur_target.as_mut())
            {
                match blur.update(target.texture(), target.size()) {
                    Ok(()) => self.blur_dirty = false,
                    Err(error) => {
                        report_warning(format!("video backdrop blur failed: {error}"));
                        self.blur_target = None;
                        if let (Some(painter), Some(id)) =
                            (self.painter.as_mut(), self.blur_texture_id.take())
                        {
                            painter.free_texture(id);
                        }
                        self.blur_dirty = false;
                    }
                }
            }
        } else if !theater && self.blur_target.is_some() {
            self.blur_target = None;
            if let (Some(painter), Some(id)) = (self.painter.as_mut(), self.blur_texture_id.take())
            {
                painter.free_texture(id);
            }
            self.blur_dirty = false;
        } else if matches!(decision, FrameDecision::Rendered(_)) {
            self.blur_dirty = true;
        }

        #[cfg(debug_assertions)]
        debug_harness::log_if_changed(
            "video surface",
            &format!(
                "renderer_available={renderer_available} target={:?} size_points={video_size_points:?}",
                self.video_target.as_ref().map(|target| target.size())
            ),
        );
        let primitives = self.egui.tessellate(output.shapes, output.pixels_per_point);
        unsafe {
            self.gl.bind_framebuffer(
                glow::FRAMEBUFFER,
                u32::try_from(self.screen_framebuffer)
                    .ok()
                    .and_then(NonZeroU32::new)
                    .map(glow::NativeFramebuffer),
            );
        }
        let Some(painter) = self.painter.as_mut() else {
            return Duration::MAX;
        };
        painter.clear(
            [self.physical_size.width, self.physical_size.height],
            [0.035, 0.047, 0.063, 1.0],
        );
        painter.paint_and_update_textures(
            [self.physical_size.width, self.physical_size.height],
            output.pixels_per_point,
            &primitives,
            &mut output.textures_delta,
        );

        #[cfg(debug_assertions)]
        debug_harness::capture_if_requested(
            &self.gl,
            self.physical_size.width,
            self.physical_size.height,
            self.video_target.as_ref(),
        );
        if let Err(error) = self.parts.surface.swap_buffers(&self.parts.context) {
            report_warning(format!("swap_buffers failed: {error}"));
            return Duration::MAX;
        }

        #[cfg(debug_assertions)]
        {
            let finished = Instant::now();
            qa_metrics::frame(qa_frame_started, finished);
            if matches!(decision, FrameDecision::Rendered(_)) {
                qa_metrics::video_presented(finished);
            }
        }

        if matches!(decision, FrameDecision::Rendered(_))
            && let Err(error) = self.mpv.report_swap()
        {
            report_warning(format!("libmpv swap report failed: {error}"));
        }

        if matches!(decision, FrameDecision::Rendered(_))
            && let Some(elapsed) = self.mpv.take_live_tune_elapsed()
        {
            tracing::info!(
                elapsed_ms = elapsed.as_millis(),
                "Live TV first video frame rendered"
            );
        }

        let mut next_repaint = output
            .viewport_output
            .get(&ViewportId::ROOT)
            .map_or(Duration::MAX, |viewport| viewport.repaint_delay);
        if let FrameDecision::WaitUntil { delay, .. } = decision {
            next_repaint = next_repaint.min(delay);
        }
        if let Some(delay) = self.report_retry_delay() {
            next_repaint = next_repaint.min(delay);
        }
        if let Some(delay) = self.mpv.stream_open_retry_delay(Instant::now()) {
            next_repaint = next_repaint.min(delay);
        }
        next_repaint
    }

    /// The playback phase and video presence the PiP model reads. The host does not observe
    /// loading states or the video track yet, so a loaded item reads as playing or paused, and
    /// video is assumed once the renderer is ready. Audio-only files are not refused yet.
    fn pip_playback(&self) -> (Option<PlaybackPhase>, bool) {
        let loaded = self.mpv.has_media() && self.mpv.has_renderer();
        let phase = loaded.then(|| match self.mpv.controls_snapshot().paused {
            Some(true) => PlaybackPhase::Paused,
            _ => PlaybackPhase::Playing,
        });
        (phase, loaded)
    }

    fn toggle_pip(&mut self, event_loop: &ActiveEventLoop) {
        if self.pip.is_pip() {
            let monitors = window_hooks::monitor_infos(event_loop);
            match self.pip.leave(&monitors) {
                Ok(transition) => self.apply_pip_transition(&transition),
                Err(_) => self.set_shell_notice("Picture-in-picture is already off.".to_owned()),
            }
            return;
        }
        let (phase, has_video) = self.pip_playback();
        let Some(normal) = window_hooks::capture_window(&self.window, event_loop) else {
            self.set_shell_notice(
                "Picture-in-picture needs the window position, which this platform did not report."
                    .to_owned(),
            );
            return;
        };
        // Write the normal window before the PiP size replaces it, so a move made within the
        // debounce window is not lost (M2-14).
        self.store_normal_window(&normal);
        match self
            .pip
            .enter(normal, PipSize::default_size(), phase, has_video)
        {
            Ok(transition) => self.apply_pip_transition(&transition),
            Err(PipError::NoVideoPlaying) => {
                self.set_shell_notice("Picture-in-picture needs a playing video.".to_owned());
            }
            Err(PipError::AlreadyInPip | PipError::NotInPip) => {}
        }
    }

    /// Perform the player actions reached from a Ctrl shortcut (Ctrl+1, Ctrl+2, Ctrl+Up/Down,
    /// Ctrl+0 and Ctrl+A). These are keyboard-only, so no button is added to the control bar.
    fn apply_player_shortcut(&mut self, action: PlayerControlAction) {
        match action {
            PlayerControlAction::FitWindowToVideo => self.fit_window_to_video(),
            PlayerControlAction::ToggleDoubleSpeed => self.toggle_double_speed(),
            PlayerControlAction::StepSpeed { up } => self.step_playback_speed(up),
            PlayerControlAction::ResetSpeed => self.reset_playback_speed(),
            PlayerControlAction::CycleAspect => self.cycle_aspect(),
            // Other actions arrive through the on-screen controls or the gamepad, not here.
            _ => {}
        }
    }

    /// Ctrl+Up / Ctrl+Down: move the playback rate one ladder notch from mpv's reported speed.
    /// The setter enforces the Watch Together speed lock, so a synchronized session stays normal.
    fn step_playback_speed(&mut self, up: bool) {
        if !self.mpv.has_media() {
            self.set_shell_notice("Playback speed needs a loaded video.".to_owned());
            return;
        }
        let current = self.mpv.controls_snapshot().speed_multiplier;
        let direction = if up { 1 } else { -1 };
        let target = kattri_client_core::controls::step_speed(current, direction);
        self.mpv.set_playback_speed(target);
    }

    /// Ctrl+0: return the playback rate to normal.
    fn reset_playback_speed(&mut self) {
        if !self.mpv.has_media() {
            self.set_shell_notice("Playback speed needs a loaded video.".to_owned());
            return;
        }
        self.mpv
            .set_playback_speed(kattri_client_core::controls::PlaybackSpeed::Normal);
    }

    /// Cycle the video aspect ratio: persist the new mode through the app, then apply it to the
    /// live player so the picture changes immediately and the next file starts the same way.
    fn cycle_aspect(&mut self) {
        if !self.mpv.has_media() {
            self.set_shell_notice("Aspect ratio needs a loaded video.".to_owned());
            return;
        }
        let mode = self.kattri_app.cycle_video_aspect();
        self.mpv.apply_video_aspect_mode(mode);
    }

    /// Ctrl+2: switch between normal and double speed. The current speed comes from mpv's own
    /// observation, and the change goes back through the speed menu's setter, so the menu remains
    /// the source of truth.
    fn toggle_double_speed(&mut self) {
        if !self.mpv.has_media() {
            self.set_shell_notice("Playback speed needs a loaded video.".to_owned());
            return;
        }
        let speed = self.mpv.controls_snapshot().speed_multiplier;
        let target = double_speed_target(speed);
        self.mpv.set_playback_speed(target);
    }

    /// Ctrl+1: ask for the decoded frame size, then resize the window when it arrives. A read is
    /// asynchronous because libmpv properties are serialized on its owner thread.
    fn fit_window_to_video(&mut self) {
        match self.mpv.request_video_frame_size() {
            VideoFrameSizeRequest::Known(size) => self.apply_one_to_one_size(size),
            VideoFrameSizeRequest::Pending => {}
            VideoFrameSizeRequest::Unavailable => self.set_shell_notice(
                "The video frame size is not known yet, so the window was not resized.".to_owned(),
            ),
        }
    }

    /// Collect a frame-size read that finished since the previous frame.
    fn apply_pending_video_frame_size(&mut self) {
        let Some(size) = self.mpv.take_video_frame_size_result() else {
            return;
        };
        match size {
            Some(size) => self.apply_one_to_one_size(size),
            None => self.set_shell_notice(
                "The video frame size could not be read, so the window was not resized.".to_owned(),
            ),
        }
    }

    /// Resize the window client area for a 1:1 video frame. `request_inner_size` with a
    /// `PhysicalSize` takes physical pixels, so the monitor scale is already accounted for: one
    /// decoded pixel becomes one screen pixel at any scale. Fullscreen and PiP own the window,
    /// so they keep their sizes and say why.
    fn apply_one_to_one_size(&mut self, frame: (u32, u32)) {
        if self.window.fullscreen().is_some() {
            self.set_shell_notice(
                "Leave fullscreen before fitting the window to the video.".to_owned(),
            );
            return;
        }
        if self.pip.is_pip() {
            self.set_shell_notice(
                "Leave picture-in-picture before fitting the window to the video.".to_owned(),
            );
            return;
        }
        let (width, height) = one_to_one_window_size(frame);
        self.window.set_maximized(false);
        let _ = self
            .window
            .request_inner_size(PhysicalSize::new(width, height));
        self.set_shell_notice(format!("Window sized to {width}x{height} for 1:1 video."));
    }

    /// Queue an mpv screenshot of the current frame (M2-15). The name comes from the settings
    /// template. The command is queued; the write is not confirmed here.
    /// Apply direct gamepad player actions after egui handles navigation and activation (M6-18).
    fn apply_gamepad_player_actions(
        &mut self,
        actions: &[kattri_client_core::focus::NavAction],
        activate_without_focus: bool,
    ) {
        if actions.is_empty() && !activate_without_focus {
            return;
        }
        self.osd_visibility.on_input(self.osd_clock.elapsed());
        let live_stream_active = self.mpv.live_stream_active();
        for action in actions {
            match player_controls::gamepad_player_action(*action) {
                GamepadPlayerAction::Control(PlayerControlAction::TogglePause)
                    if live_stream_active =>
                {
                    self.mpv.stop_playback();
                }
                GamepadPlayerAction::Control(PlayerControlAction::TogglePause) => {
                    self.mpv.toggle_paused();
                }
                GamepadPlayerAction::Control(PlayerControlAction::SeekRelative(delta)) => {
                    self.mpv.seek_relative(delta);
                }
                GamepadPlayerAction::Control(_)
                | GamepadPlayerAction::Navigate(_)
                | GamepadPlayerAction::ActivateFocused => {}
                GamepadPlayerAction::Leave => self.mpv.stop_playback(),
                GamepadPlayerAction::ToggleMediaInfo => self
                    .mpv
                    .dispatch_playback_key(mpv_host::PlaybackKeyAction::MediaInfo),
            }
        }
        if activate_without_focus {
            if live_stream_active {
                self.mpv.stop_playback();
            } else {
                self.mpv.toggle_paused();
            }
        }
        self.window.request_redraw();
    }

    /// Refresh-rate matching (M6-04): switch for a newly loaded file, restore after playback.
    fn update_display_switch(&mut self) {
        let mut notice = None;
        if let Some((rate, avoid_25_30)) = self.mpv.take_content_rate() {
            notice = self.display_switch.apply(rate, avoid_25_30);
        }
        let restore_notice = self
            .display_switch
            .tick(self.mpv.has_media(), Instant::now());
        if let Some(text) = notice.or(restore_notice) {
            self.set_shell_notice(text);
        }
    }

    fn take_screenshot(&mut self) {
        const NOTHING_TO_CAPTURE: &str = "Nothing to capture yet.";
        let Some(session) = self.session else {
            self.set_shell_notice(NOTHING_TO_CAPTURE.to_owned());
            return;
        };
        if !(self.mpv.has_renderer() && self.mpv.has_media()) {
            self.set_shell_notice(NOTHING_TO_CAPTURE.to_owned());
            return;
        }
        let (configured, template) = {
            let screenshot = &self.kattri_app.video_settings().screenshot;
            (screenshot.directory.clone(), screenshot.template.clone())
        };
        let directory = match screenshot_directory(&configured) {
            Ok(directory) => directory,
            Err(message) => {
                self.set_shell_notice(message.to_owned());
                return;
            }
        };
        if std::fs::create_dir_all(&directory).is_err() {
            self.set_shell_notice("The screenshot folder could not be created.".to_owned());
            return;
        }
        let existing = match existing_file_names(&directory) {
            Ok(names) => names,
            Err(_) => {
                self.set_shell_notice("The screenshot folder could not be read.".to_owned());
                return;
            }
        };
        let stem_source = self
            .mpv
            .playing_item_id()
            .map(str::to_owned)
            .or_else(|| self.local_stem.clone())
            .unwrap_or_else(|| "video".to_owned());
        let position = self
            .mpv
            .controls_snapshot()
            .position_seconds
            .and_then(duration_from_seconds)
            .unwrap_or_default();
        self.screenshot_count = self.screenshot_count.saturating_add(1);
        let context = ScreenshotContext {
            file_stem: &stem_source,
            position,
            counter: self.screenshot_count,
        };
        let stem = match screenshot_stem(&template, &context) {
            Ok(stem) => stem,
            Err(_) => {
                self.set_shell_notice("The screenshot name template is invalid.".to_owned());
                return;
            }
        };
        let file_name = match unique_file_name(&stem, ScreenshotFormat::Png, &existing) {
            Ok(name) => name,
            Err(_) => {
                self.set_shell_notice("No free screenshot file name is left.".to_owned());
                return;
            }
        };
        match session.screenshot_to_file(
            &directory,
            &template,
            &context,
            &existing,
            PROVISIONAL_SCREENSHOT_MODE,
        ) {
            Ok(command) => {
                self.mpv.expect_screenshot(command);
                self.set_shell_notice(format!("Screenshot requested: {file_name}"));
            }
            Err(error) => self.set_shell_notice(format!("Screenshot failed: {error}")),
        }
    }

    /// Read once at start-up; changing the setting takes effect on the next launch.
    #[cfg(windows)]
    fn global_shortcuts_enabled(&self) -> bool {
        self.kattri_app.global_shortcuts_enabled()
    }

    fn set_shell_notice(&mut self, text: String) {
        self.shell_notice = Some((text, self.osd_clock.elapsed()));
    }

    /// Hold a sleep/display requirement while media is loaded, and release it when playback
    /// stops. The platform guard is created and dropped on this UI thread.
    fn update_sleep_inhibit(&mut self) {
        let playing = self.mpv.is_playing();
        let now = Instant::now();
        if !playing {
            self.power_guard = None;
            self.power_retry_at = None;
            self.power_error_logged = false;
            return;
        }
        if let Some(PowerStatus::Failed(error)) = self.power_guard.as_ref().map(PowerGuard::status)
        {
            self.power_guard = None;
            self.power_retry_at = now.checked_add(Duration::from_secs(5));
            if !self.power_error_logged {
                tracing::warn!(%error, "sleep inhibition could not be acquired");
                self.power_error_logged = true;
            }
        }
        if self.power_guard.is_none() && self.power_retry_at.is_none_or(|retry_at| now >= retry_at)
        {
            match self.power.inhibit_sleep("Playing video") {
                Ok(guard) => {
                    self.power_guard = Some(guard);
                    self.power_retry_at = None;
                }
                Err(error) => {
                    self.power_retry_at = now.checked_add(Duration::from_secs(5));
                    if !self.power_error_logged {
                        tracing::warn!(%error, "sleep inhibition is unavailable");
                        self.power_error_logged = true;
                    }
                }
            }
        }
    }

    fn power_poll_deadline(&self) -> Option<Instant> {
        if matches!(
            self.power_guard.as_ref().map(PowerGuard::status),
            Some(PowerStatus::Pending)
        ) {
            Instant::now().checked_add(Duration::from_millis(100))
        } else {
            self.power_retry_at
        }
    }

    /// Report the queued screenshot's real outcome once libmpv answers. Queueing alone does not
    /// prove the file was written (unwritable folder, render-mode capture failure).
    fn poll_screenshot_result(&mut self) {
        let Some(error) = self.mpv.take_screenshot_result() else {
            return;
        };
        if error < 0 {
            self.set_shell_notice(format!("Screenshot failed (libmpv error {error})."));
        } else {
            self.set_shell_notice("Screenshot saved.".to_owned());
        }
    }

    /// Reallocate the video FBO only once the requested size has settled. A fullscreen toggle
    /// changes the size on several consecutive frames; applying each one reallocated and cleared
    /// the texture, which showed up as a flash of black or garbage during the transition.
    fn apply_video_target_size(&mut self, size: [u32; 2]) -> Result<(), String> {
        const SETTLE: Duration = Duration::from_millis(150);
        let current = self.video_target.as_ref().map(VideoTarget::size);
        if current == Some(size) {
            self.pending_video_target = None;
            return Ok(());
        }
        let now = Instant::now();
        match self.pending_video_target {
            Some((pending, since)) if pending == size && now.duration_since(since) >= SETTLE => {
                self.pending_video_target = None;
                self.resize_video_target(size)
            }
            _ => {
                // Keep the previous frame on screen, stretched, until the size settles.
                self.pending_video_target = Some((size, now));
                self.egui.request_repaint_after(Duration::from_millis(40));
                Ok(())
            }
        }
    }

    fn resize_video_target(&mut self, size: [u32; 2]) -> Result<(), String> {
        if let Some(current) = self.video_target.as_mut() {
            // Reuse the registered texture and FBO. Allocating a new target per resize and
            // handing the old texture to egui_glow would queue one full-size texture for
            // every frame of a drag until shutdown.
            let changed = current.size() != size;
            let result = current.resize(&self.gl, size);
            if changed && result.is_ok() {
                self.blur_dirty = true;
            }
            return result;
        }
        let next = VideoTarget::new(&self.gl, size)?;
        let painter = self
            .painter
            .as_mut()
            .ok_or_else(|| "egui_glow painter has already been destroyed".to_owned())?;
        painter.replace_native_texture(self.video_texture_id, next.texture());
        self.video_target = Some(next);
        self.blur_dirty = true;
        Ok(())
    }

    fn destroy(&mut self) -> bool {
        // Never leave the desktop with an invisible pointer after shutdown.
        self.show_cursor();
        if self.destroyed {
            return true;
        }
        if let Err(error) = self.mpv.close() {
            // A lost GL context makes a correct libmpv teardown impossible. Vetoing the close
            // forever would leave a window the user cannot dismiss, so finish the teardown
            // without GL and let the renderer leak its callback owners (it already refuses to
            // free pointers libmpv may still hold).
            self.shutdown_error = Some(format!(
                "libmpv could not be closed because the GL context is unavailable: {error}"
            ));
            self.destroyed = true;
            self.painter.take();
            return true;
        }
        self.mpv.finish_playback_reporting();
        self.flush_playback_reports();
        if let Err(error) = self.parts.make_current() {
            self.shutdown_error = Some(format!(
                "Could not make the host GL context current for teardown: {error}"
            ));
            self.destroyed = true;
            self.painter.take();
            return true;
        }
        if let Some(target) = self.video_target.take() {
            target.destroy_framebuffer(&self.gl);
        }
        self.blur_target.take();
        if let Some(painter) = self.painter.as_mut() {
            painter.destroy();
        }
        self.painter.take();
        self.destroyed = true;
        true
    }

    fn flush_playback_reports(&mut self) {
        let app = &mut self.kattri_app;
        self.mpv
            .flush_report_actions(self.osd_clock.elapsed(), |action| {
                app.report_playback_action(action)
            });
    }

    fn sync_report_connection(&mut self) {
        let generation = self.kattri_app.connection_generation();
        if generation == self.report_connection_generation {
            return;
        }
        self.report_connection_generation = generation;
        self.sync_play_bridge.clear();
        let had_context = self.mpv.has_reporting_context();
        let counts = self.mpv.clear_reporting_for_connection_change();
        self.report_connection_notice = had_context.then(|| {
            format!(
                "Jellyfin connection changed. Reports from the previous sign-in were discarded (Start: {}, Progress: {}, Stop: {}). Current playback continues without session reporting.",
                counts.started, counts.progress, counts.stopped,
            )
        });
    }

    fn report_retry_delay(&self) -> Option<Duration> {
        self.mpv.report_retry_delay(self.osd_clock.elapsed())
    }

    fn poll_sync_play_status(&mut self) {
        if !debug_validation::should_poll_syncplay_status(
            self.mpv.reporting_enabled(),
            self.syncplay_status_opt_in,
        ) {
            self.sync_play_bridge.clear();
            return;
        }
        for (request_id, accepted) in self.kattri_app.take_sync_play_status_results() {
            self.sync_play_bridge
                .completed(request_id, accepted, Instant::now());
        }
        let sample = self.mpv.sync_play_sample();
        // Observe every sample, not just a restart: pause, unpause and seek changes during normal
        // playback must reach the group even when stream-only validation keeps ordinary playback
        // reporting disabled. The observer itself filters noise and rate-limits corrections.
        if sample.loaded
            && !sample.resume_pending
            && sample.cache_paused != Some(true)
            && let (Some(generation), Some(paused), Some(position)) =
                (sample.generation, sample.paused, sample.position_ticks)
        {
            self.kattri_app
                .observe_sync_play_player(generation, paused, position);
        }
        let Some(report) = self.sync_play_bridge.poll(sample, Instant::now()) else {
            return;
        };
        let result = if report.buffering {
            self.kattri_app.sync_play_buffering(
                report.queue_generation,
                &report.playlist_item_id,
                report.position_ticks,
                report.is_playing,
            )
        } else {
            self.kattri_app.sync_play_ready(
                report.queue_generation,
                &report.playlist_item_id,
                report.position_ticks,
                report.is_playing,
            )
        };
        match result {
            Ok(request_id) => self.sync_play_bridge.submitted(request_id, report),
            Err(
                jellyfin_egui_client::PlaybackReportSubmitError::SyncPlayNotJoined
                | jellyfin_egui_client::PlaybackReportSubmitError::SyncPlayStatusInvalid,
            ) => {
                self.sync_play_bridge.clear();
            }
            Err(_) => {}
        }
    }

    fn begin_close(&mut self) {
        if self.mpv.has_media() {
            self.mpv.stop_playback();
        }
        // Put the display back before the window goes away.
        let _ = self.display_switch.restore_now();
        self.mpv.finish_playback_reporting();
        self.report_close_grace.begin(self.osd_clock.elapsed());
    }

    fn advance_close(&mut self) -> bool {
        self.flush_playback_reports();
        match self
            .report_close_grace
            .advance(self.osd_clock.elapsed(), self.mpv.has_pending_reports())
        {
            CloseAdmission::Ready => {
                self.shutdown_error = None;
                self.destroy()
            }
            CloseAdmission::Retry => false,
            CloseAdmission::Idle => false,
            CloseAdmission::TimedOut => {
                let counts = self.mpv.pending_report_counts();
                self.shutdown_error = Some(format!(
                    "Close delayed: playback reports have not been accepted (Start: {}, Progress: {}, Stop: {}). They remain retained. Retry closing after the network queue recovers.",
                    counts.started, counts.progress, counts.stopped,
                ));
                self.window.request_redraw();
                false
            }
        }
    }
}

/// Everything the on-screen controls need for one frame of the theater player.
struct OsdState<'a> {
    position: Duration,
    duration: Option<Duration>,
    paused: Option<bool>,
    speed_multiplier: Option<f64>,
    media_active: bool,
    previous_track_enabled: bool,
    next_track_enabled: bool,
    chapters: &'a [ChapterChoice],
    secondary_subtitles: &'a [SecondarySubtitleChoice],
    secondary_selected: Option<i64>,
    pip_active: bool,
    pip_available: bool,
    title: Option<&'a str>,
    subtitle: Option<&'a str>,
    opacity: f32,
    volume_percent: Option<f64>,
    volume_max_percent: u32,
    muted: Option<bool>,
    fullscreen: bool,
    skip_settings: Option<kattri_client_core::segments::SkipSettings>,
    feedback: Option<&'a mpv_host::ControlsFeedback>,
    audio_tracks: Option<&'a [mpv_host::TrackMenuOption]>,
    subtitle_tracks: Option<&'a [mpv_host::TrackMenuOption]>,
    burn_in_subtitle_tracks: Option<&'a [player_controls::TrackChoice<Option<i32>>]>,
    burn_in_subtitle_feedback: Option<&'a str>,
    track_feedback: Option<&'a str>,
}

/// How long before the end of an episode the Up Next card appears.
const UP_NEXT_LEAD: Duration = Duration::from_secs(30);

/// Whether the Up Next card is due at `position`. Short videos never show it.
fn up_next_due(position: Duration, duration: Duration) -> bool {
    duration > UP_NEXT_LEAD * 4 && position + UP_NEXT_LEAD >= duration
}

/// Near the end of an episode with a next one set to auto-play: names it, counts down, and
/// offers Play now and Cancel.
fn up_next_card(
    ctx: &egui::Context,
    mpv: &mut MpvHost<'_>,
    kattri_app: &mut KattriApp,
    osd: &OsdState<'_>,
    backdrop: Option<blur::BackdropBlur>,
) -> bool {
    let (Some(duration), Some(playing)) = (osd.duration, mpv.playing_item_id().map(str::to_owned))
    else {
        return false;
    };
    if !up_next_due(osd.position, duration) {
        return false;
    }
    let Some(label) = kattri_app.up_next_label(&playing).map(str::to_owned) else {
        return false;
    };
    let remaining = duration.saturating_sub(osd.position).as_secs();
    egui::Area::new(egui::Id::unique("up_next_card"))
        .order(egui::Order::Foreground)
        .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-28.0, -150.0))
        .show(ctx, |ui| {
            blur::show_frosted_glass_frame(ui, backdrop, 1.0, egui::Margin::same(16), |ui| {
                ui.set_max_width(340.0);
                ui.label(
                    egui::RichText::new("Up next")
                        .size(13.0)
                        .color(egui::Color32::from_gray(190)),
                );
                ui.label(
                    egui::RichText::new(&label)
                        .size(17.0)
                        .strong()
                        .color(egui::Color32::WHITE),
                );
                ui.label(
                    egui::RichText::new(format!("Starts in {remaining} s"))
                        .color(egui::Color32::from_gray(190)),
                );
                ui.horizontal(|ui| {
                    if ui.button("Play now").clicked() {
                        kattri_app.on_episode_next(&playing);
                    }
                    if ui.button("Cancel").clicked() {
                        kattri_app.cancel_up_next(&playing);
                    }
                });
            });
        });
    true
}

/// Draws the playback controls and performs the actions they return.
fn osd_controls(
    ui: &mut egui::Ui,
    mpv: &mut MpvHost<'_>,
    kattri_app: &mut KattriApp,
    osd: &OsdState<'_>,
    pip_toggle_requested: &mut bool,
    fullscreen_toggle_requested: &mut bool,
    backdrop: Option<blur::BackdropBlur>,
) {
    let live_stream_active = mpv.live_stream_active();
    // Reads happen before the skipper takes a mutable borrow of the host for the draw.
    let subtitle_selected = mpv.subtitle_track_selected();
    let subtitle_delay_seconds = mpv.subtitle_delay_seconds();
    let (sync_feedback_text, sync_feedback_tone) = {
        let (msg, tone) = mpv.sync_feedback().display();
        (msg.to_owned(), tone)
    };
    let playing_item_id = mpv.playing_item_id().map(str::to_owned);
    let skip = osd.skip_settings.map(|settings| SkipControls {
        skipper: mpv.segment_skipper_mut(),
        settings,
    });
    // The popover is built during the draw and the host performs the chosen actions afterwards:
    // the panel is drawn while the skipper still holds the host.
    let mut search_subtitles = false;
    let mut sync_marker = None;
    let mut delay_nudge: Option<f64> = None;
    let mut reset_delay = false;
    // Live TV has no item subtitles to search for.
    let subtitle_search_available =
        kattri_app.can_manage_subtitles() && playing_item_id.is_some() && !live_stream_active;
    let actions = player_controls::draw_with_skip_and_extra(
        ui,
        PlayerControlsView {
            position: osd.position,
            duration: osd.duration,
            paused: osd.paused,
            speed_multiplier: osd.speed_multiplier,
            speed_enabled: !kattri_app.sync_play_speed_locked(),
            aspect_label: kattri_app.video_settings().aspect.label(),
            seek_enabled: true,
            stop_enabled: osd.media_active,
            previous_track_enabled: osd.previous_track_enabled,
            next_track_enabled: osd.next_track_enabled,
            chapters: osd.chapters,
            secondary_subtitles: osd.secondary_subtitles,
            secondary_selected: osd.secondary_selected,
            pip_active: osd.pip_active,
            pip_available: osd.pip_available,
            volume_percent: osd.volume_percent,
            volume_max_percent: osd.volume_max_percent,
            muted: osd.muted,
            fullscreen: osd.fullscreen,
            // mpv does not currently expose a demuxer-cache range through the properties this
            // host observes, so the loaded range is drawn only when a backend supplies it.
            buffered: None,
            audio_tracks: osd.audio_tracks,
            subtitle_tracks: osd.subtitle_tracks,
            burn_in_subtitle_tracks: osd.burn_in_subtitle_tracks,
            opacity: osd.opacity,
        },
        skip,
        |ui| {
            ui.label(
                egui::RichText::new("Subtitle sync")
                    .size(11.0)
                    .strong()
                    .color(player_controls::OSD_MUTED),
            );
            if let Some(feedback) = osd.burn_in_subtitle_feedback.or(osd.track_feedback) {
                ui.small(feedback);
            }
            let delay_text = subtitle_delay_seconds.map_or_else(
                || "Delay: --".to_owned(),
                |secs| format!("Delay: {secs:+.2}s"),
            );
            ui.label(
                egui::RichText::new(delay_text)
                    .size(12.0)
                    .color(player_controls::OSD_MUTED),
            );
            let sync_color = match sync_feedback_tone {
                mpv_host::SyncFeedbackTone::Error => egui::Color32::LIGHT_RED,
                mpv_host::SyncFeedbackTone::Success => egui::Color32::from_rgb(130, 220, 150),
                mpv_host::SyncFeedbackTone::Guidance | mpv_host::SyncFeedbackTone::Pending => {
                    player_controls::OSD_MUTED
                }
            };
            ui.add(
                egui::Label::new(
                    egui::RichText::new(&sync_feedback_text)
                        .size(11.5)
                        .color(sync_color),
                )
                .wrap(),
            );
            ui.horizontal_wrapped(|ui| {
                for (marker, label) in [
                    (CalibrationMarker::VoiceHeard, "Voice heard"),
                    (CalibrationMarker::TextSeen, "Text seen"),
                ] {
                    if player_controls::sync_marker_button_for(
                        ui,
                        marker,
                        label,
                        subtitle_selected,
                        osd.opacity,
                        136.0,
                    ) {
                        sync_marker = Some(marker);
                    }
                }
            });
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(subtitle_selected, egui::Button::new("-100ms"))
                    .clicked()
                {
                    delay_nudge = Some(-0.1);
                }
                if ui
                    .add_enabled(subtitle_selected, egui::Button::new("+100ms"))
                    .clicked()
                {
                    delay_nudge = Some(0.1);
                }
                if ui
                    .add_enabled(subtitle_selected, egui::Button::new("Reset"))
                    .clicked()
                {
                    reset_delay = true;
                }
            });
            if subtitle_search_available
                && ui
                    .button(jellyfin_egui_client::search_subtitles_label())
                    .clicked()
            {
                search_subtitles = true;
                ui.close();
            }
        },
        |time| {
            playing_item_id
                .as_deref()
                .and_then(|item_id| kattri_app.trickplay_thumbnail(item_id, time))
        },
        backdrop,
    );
    if let Some(marker) = sync_marker {
        mpv.capture_sync_marker(marker);
    }
    if let Some(delta) = delay_nudge {
        mpv.nudge_subtitle_delay(delta);
    }
    if reset_delay {
        mpv.reset_subtitle_delay();
    }
    if search_subtitles && let Some(item_id) = playing_item_id.as_deref() {
        kattri_app.open_subtitle_search(item_id, true);
    }
    if live_stream_active {
        ui.small("Live TV cannot pause; the Pause button stops the channel.");
    }
    for action in actions {
        match action {
            PlayerControlAction::SeekRelative(delta) => mpv.seek_relative(delta),
            PlayerControlAction::SeekAbsolute(target) => mpv.seek_absolute(target),
            PlayerControlAction::TogglePause if live_stream_active => mpv.stop_playback(),
            PlayerControlAction::TogglePause => mpv.toggle_paused(),
            PlayerControlAction::PreviousTrack => {
                if let Some(playing_item_id) = mpv.playing_item_id().map(str::to_owned) {
                    if kattri_app.music_queue_current_item_id() == Some(playing_item_id.as_str()) {
                        if kattri_app.on_music_previous(&playing_item_id, osd.position) {
                            mpv.seek_absolute(Duration::ZERO);
                        }
                    } else {
                        kattri_app.on_episode_previous(&playing_item_id);
                    }
                }
            }
            PlayerControlAction::NextTrack => {
                if let Some(playing_item_id) = mpv.playing_item_id().map(str::to_owned) {
                    if kattri_app.music_queue_current_item_id() == Some(playing_item_id.as_str()) {
                        kattri_app.on_music_next(&playing_item_id);
                    } else {
                        kattri_app.on_episode_next(&playing_item_id);
                    }
                }
            }
            PlayerControlAction::Stop => mpv.stop_playback(),
            PlayerControlAction::SetSpeed(speed) => mpv.set_playback_speed(speed),
            PlayerControlAction::SelectSecondarySubtitle(track) => {
                mpv.select_secondary_subtitle(track);
            }
            PlayerControlAction::SelectAudioTrack(selection) => {
                mpv.select_audio_track(selection);
            }
            PlayerControlAction::SelectSubtitleTrack(selection) => {
                mpv.select_subtitle_track(selection);
            }
            PlayerControlAction::SelectBurnInSubtitle(stream_index) => {
                if kattri_app
                    .select_burn_in_subtitle(stream_index)
                    .is_some_and(|was_paused| !was_paused)
                {
                    mpv.set_paused(true);
                }
            }
            PlayerControlAction::ToggleMediaInfo => {
                mpv.dispatch_playback_key(mpv_host::PlaybackKeyAction::MediaInfo);
            }
            PlayerControlAction::TogglePip => *pip_toggle_requested = true,
            PlayerControlAction::StepFrame { backward } => {
                mpv.dispatch_playback_key(mpv_host::PlaybackKeyAction::FrameStep { backward });
            }
            PlayerControlAction::SetVolume(percent) => {
                mpv.set_volume_percent(percent, osd.volume_max_percent);
            }
            PlayerControlAction::ToggleMute => mpv.toggle_mute(),
            PlayerControlAction::StepChapter { forward } => mpv.step_chapter_control(forward),
            PlayerControlAction::ToggleFullscreen => *fullscreen_toggle_requested = true,
            PlayerControlAction::CycleAspect => {
                let mode = kattri_app.cycle_video_aspect();
                mpv.apply_video_aspect_mode(mode);
            }
            PlayerControlAction::SetAspect(mode) => {
                if kattri_app.video_settings().aspect != mode {
                    for _ in 0..5 {
                        if kattri_app.cycle_video_aspect() == mode {
                            break;
                        }
                    }
                }
                mpv.apply_video_aspect_mode(mode);
            }
            // Ctrl shortcuts are handled before the OSD is drawn; the speed steps and reset reach
            // the player through apply_player_shortcut, not through the on-screen action loop.
            PlayerControlAction::FitWindowToVideo
            | PlayerControlAction::ToggleDoubleSpeed
            | PlayerControlAction::StepSpeed { .. }
            | PlayerControlAction::ResetSpeed => {}
        }
    }
    if let Some(feedback) = osd.feedback {
        let color = if feedback.is_error {
            egui::Color32::LIGHT_RED
        } else {
            player_controls::OSD_MUTED
        };
        ui.colored_label(color, &feedback.message);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TheaterEscapeAction {
    None,
    ClosePopup,
    CloseSubtitleSearch,
    CloseMediaInfo,
    StopPlayback,
}

fn theater_escape_action(
    ctx: &egui::Context,
    escape_pressed: bool,
    typing: bool,
    subtitle_search_open: bool,
    media_info_open: bool,
) -> TheaterEscapeAction {
    if !escape_pressed {
        return TheaterEscapeAction::None;
    }
    if egui::Popup::is_any_open(ctx) {
        egui::Popup::close_all(ctx);
        TheaterEscapeAction::ClosePopup
    } else if subtitle_search_open {
        TheaterEscapeAction::CloseSubtitleSearch
    } else if typing {
        TheaterEscapeAction::None
    } else if media_info_open {
        TheaterEscapeAction::CloseMediaInfo
    } else {
        TheaterEscapeAction::StopPlayback
    }
}

fn playback_shortcuts_enabled(typing: bool, popup_open: bool) -> bool {
    !typing && !popup_open
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HostShortcutAction {
    ToggleFullscreen,
    Minimize,
    ToggleAlwaysOnTop,
    ToggleTvMode,
}

fn handle_winit_media_key(
    host: &mut GlHost,
    key_event: &winit::event::KeyEvent,
    playing_item_id: &str,
) -> bool {
    use winit::keyboard::{Key, KeyCode, NamedKey, PhysicalKey};

    match (&key_event.logical_key, key_event.physical_key) {
        (Key::Named(NamedKey::MediaPlayPause), _)
        | (_, PhysicalKey::Code(KeyCode::MediaPlayPause)) => {
            host.mpv.toggle_paused();
            true
        }
        (Key::Named(NamedKey::MediaStop), _) | (_, PhysicalKey::Code(KeyCode::MediaStop)) => {
            host.mpv.stop_playback();
            true
        }
        (Key::Named(NamedKey::MediaTrackNext), _)
        | (_, PhysicalKey::Code(KeyCode::MediaTrackNext)) => {
            if host.kattri_app.music_queue_current_item_id() == Some(playing_item_id) {
                host.kattri_app.on_music_next(playing_item_id);
            } else {
                host.kattri_app.on_episode_next(playing_item_id);
            }
            true
        }
        (Key::Named(NamedKey::MediaTrackPrevious), _)
        | (_, PhysicalKey::Code(KeyCode::MediaTrackPrevious)) => {
            if host.kattri_app.music_queue_current_item_id() == Some(playing_item_id) {
                let position = host
                    .mpv
                    .controls_snapshot()
                    .position_seconds
                    .and_then(duration_from_seconds)
                    .unwrap_or_default();
                if host.kattri_app.on_music_previous(playing_item_id, position) {
                    host.mpv.seek_absolute(Duration::ZERO);
                }
            } else {
                host.kattri_app.on_episode_previous(playing_item_id);
            }
            true
        }
        _ => false,
    }
}

fn host_shortcut_action(events: &[egui::Event]) -> Option<HostShortcutAction> {
    events.iter().find_map(|event| {
        let egui::Event::Key {
            key,
            pressed: true,
            repeat: false,
            modifiers,
            ..
        } = event
        else {
            return None;
        };
        if modifiers.mac_cmd {
            return None;
        }
        match key {
            egui::Key::F11 if !modifiers.ctrl && !modifiers.shift && !modifiers.alt => {
                Some(HostShortcutAction::ToggleFullscreen)
            }
            egui::Key::Enter if modifiers.alt && !modifiers.ctrl && !modifiers.shift => {
                Some(HostShortcutAction::ToggleFullscreen)
            }
            egui::Key::M if modifiers.ctrl && !modifiers.shift && !modifiers.alt => {
                Some(HostShortcutAction::Minimize)
            }
            egui::Key::A if modifiers.ctrl && modifiers.shift && !modifiers.alt => {
                Some(HostShortcutAction::ToggleAlwaysOnTop)
            }
            egui::Key::W if modifiers.ctrl && modifiers.shift && !modifiers.alt => {
                Some(HostShortcutAction::ToggleTvMode)
            }
            _ => None,
        }
    })
}

/// Keep keyboard focus and open player menus visible beyond the pointer inactivity timer.
fn player_controls_visible(timed: bool, focused: bool, popup_open: bool) -> bool {
    player_controls::player_controls_visible(timed, focused, popup_open)
}

fn append_gamepad_navigation_events(
    input: &mut egui::RawInput,
    actions: &[kattri_client_core::focus::NavAction],
    has_focus: bool,
) -> bool {
    let mut initial_focus_queued = false;
    let has_navigation = actions.iter().any(|action| {
        matches!(
            player_controls::gamepad_player_action(*action),
            GamepadPlayerAction::Navigate(_)
        )
    });
    let mut activate_without_focus = false;

    for action in actions {
        match player_controls::gamepad_player_action(*action) {
            GamepadPlayerAction::Navigate(key) if has_focus => {
                input.events.push(gamepad_key_event(key));
            }
            GamepadPlayerAction::Navigate(_) if !initial_focus_queued => {
                // The first directional press gives the player surface focus; a later press
                // moves spatially within the visible controls.
                input.events.push(gamepad_key_event(egui::Key::Tab));
                initial_focus_queued = true;
            }
            GamepadPlayerAction::Navigate(_) => {}
            GamepadPlayerAction::ActivateFocused if has_focus && !has_navigation => {
                input.events.push(gamepad_key_event(egui::Key::Enter));
            }
            GamepadPlayerAction::ActivateFocused
                if !has_focus && !initial_focus_queued && !has_navigation =>
            {
                activate_without_focus = true;
            }
            GamepadPlayerAction::ActivateFocused => {}
            GamepadPlayerAction::Control(_)
            | GamepadPlayerAction::Leave
            | GamepadPlayerAction::ToggleMediaInfo => {}
        }
    }

    activate_without_focus
}

fn gamepad_key_event(key: egui::Key) -> egui::Event {
    egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    }
}

/// The window client size that shows a decoded frame at 1:1. The theater draws the video into a
/// 16:9 box, so the window must be the smallest 16:9 box that contains the frame; a wide film
/// then has black bars above and below rather than being scaled down. Integer arithmetic keeps
/// the box exact at the common 16:9 sizes.
fn one_to_one_window_size((width, height): (u32, u32)) -> (u32, u32) {
    let width = u64::from(width.max(1));
    let height = u64::from(height.max(1));
    let (outer_width, outer_height) = if width * 9 >= height * 16 {
        (width, (width * 9).div_ceil(16))
    } else {
        ((height * 16).div_ceil(9), height)
    };
    (
        u32::try_from(outer_width).unwrap_or(u32::MAX),
        u32::try_from(outer_height).unwrap_or(u32::MAX),
    )
}

/// The speed Ctrl+2 selects: double unless the player is already there, in which case normal.
/// Anything else, including an unknown speed, goes to double as the shortcut promises.
fn double_speed_target(current: Option<f64>) -> PlaybackSpeed {
    let double = PlaybackSpeed::Double.multiplier();
    if current.is_some_and(|value| (value - double).abs() < 0.001) {
        PlaybackSpeed::Normal
    } else {
        PlaybackSpeed::Double
    }
}

/// The largest 16:9 box that fits `available`.
fn fit_16_9(available: egui::Vec2) -> egui::Vec2 {
    let width = available.x.max(1.0);
    let height = available.y.max(1.0);
    if width * 9.0 <= height * 16.0 {
        egui::vec2(width, width * 9.0 / 16.0)
    } else {
        egui::vec2(height * 16.0 / 9.0, height)
    }
}

fn label_video_surface(response: &egui::Response, title: Option<&str>) {
    let label = title.map_or_else(
        || "Video playback surface".to_owned(),
        |title| format!("Video playback surface: {title}"),
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Image, true, label.clone()));
}

/// A bare video click toggles once. Overlay controls, an open popup, or the details surface own
/// their clicks, and the center-play affordance is counted separately so it cannot toggle twice.
fn video_click_toggles(
    video_response: &egui::Response,
    center_play_clicked: bool,
    popup_open: bool,
    media_info_open: bool,
    subtitle_search_open: bool,
) -> bool {
    video_response.clicked_by(egui::PointerButton::Primary)
        && !center_play_clicked
        && !popup_open
        && !media_info_open
        && !subtitle_search_open
}

/// Whether the bottom status bar has anything to show.
fn status_bar_visible(media_active: bool, renderer_available: bool, notice_showing: bool) -> bool {
    media_active || !renderer_available || notice_showing
}

fn player_surface_active(
    media_active: bool,
    playback_preparing: bool,
    mpv_loading: bool,
    pip_active: bool,
) -> bool {
    let _ = pip_active;
    media_active || playback_preparing || mpv_loading
}

fn theater_skeleton_visible(playback_preparing: bool, mpv_loading: bool) -> bool {
    playback_preparing || mpv_loading
}

fn should_use_theater_view(renderer_available: bool, player_surface_active: bool) -> bool {
    renderer_available && player_surface_active
}

/// Dim player-bar skeleton shown while stream negotiation is in flight so the theater frame
/// reads immediately on Play press before libmpv opens the media.
fn draw_skeleton_bottom_bar(ctx: &egui::Context, stage: egui::Rect, screen: egui::Rect) {
    egui::Area::new(egui::Id::unique("theater_skeleton_bottom"))
        .order(egui::Order::Foreground)
        .anchor(
            egui::Align2::LEFT_BOTTOM,
            egui::vec2(
                stage.left() - screen.left(),
                stage.bottom() - screen.bottom(),
            ),
        )
        .show(ctx, |ui| {
            ui.set_width(stage.width());
            let margin = if stage.width() < 360.0 { 10 } else { 18 };
            egui::Frame::new()
                .inner_margin(egui::Margin {
                    left: margin,
                    right: margin,
                    top: 6,
                    bottom: 14,
                })
                .show(ui, |ui| {
                    let rail_color = egui::Color32::from_white_alpha(26);
                    let item_color = egui::Color32::from_white_alpha(22);
                    let rail_width = ui.available_width().max(1.0);
                    let (rail_rect, _) =
                        ui.allocate_exact_size(egui::vec2(rail_width, 26.0), egui::Sense::hover());
                    let track = egui::Rect::from_center_size(
                        rail_rect.center(),
                        egui::vec2(rail_rect.width(), 3.0),
                    );
                    ui.painter()
                        .rect_filled(track, egui::CornerRadius::same(2), rail_color);
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(6.0, 4.0);
                        let (play_rect, _) =
                            ui.allocate_exact_size(egui::vec2(44.0, 44.0), egui::Sense::hover());
                        ui.painter()
                            .circle_filled(play_rect.center(), 22.0, item_color);
                        if stage.width() >= 350.0 {
                            for _ in 0..2 {
                                let (rect, _) = ui.allocate_exact_size(
                                    egui::vec2(32.0, 32.0),
                                    egui::Sense::hover(),
                                );
                                ui.painter().rect_filled(
                                    rect,
                                    egui::CornerRadius::same(8),
                                    item_color,
                                );
                            }
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.spacing_mut().item_spacing = egui::vec2(6.0, 4.0);
                            let right_count = if stage.width() >= 470.0 { 6 } else { 4 };
                            for _ in 0..right_count {
                                let (rect, _) = ui.allocate_exact_size(
                                    egui::vec2(32.0, 32.0),
                                    egui::Sense::hover(),
                                );
                                ui.painter().rect_filled(
                                    rect,
                                    egui::CornerRadius::same(8),
                                    item_color,
                                );
                            }
                        });
                    });
                });
        });
}

/// The folder for screenshots. An empty setting means the Pictures folder of the user profile.
/// A relative path is refused, because mpv would resolve it against its own working directory.
fn screenshot_directory(configured: &str) -> Result<PathBuf, &'static str> {
    let configured = configured.trim();
    if configured.is_empty() {
        let home = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .ok_or("No user folder is known, so screenshots need a folder in Settings.")?;
        return Ok(PathBuf::from(home).join("Pictures"));
    }
    let path = PathBuf::from(configured);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err("The screenshot folder in Settings must be an absolute path.")
    }
}

fn existing_file_names(directory: &Path) -> std::io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        if let Some(name) = entry?.file_name().to_str() {
            names.push(name.to_owned());
        }
    }
    Ok(names)
}

#[cfg(test)]
mod shell_tests {
    #[test]
    fn playback_controls_do_not_hide_focused_widgets_or_open_menus() {
        assert!(!super::player_controls_visible(false, false, false));
        assert!(super::player_controls_visible(true, false, false));
        assert!(super::player_controls_visible(false, true, false));
        assert!(super::player_controls_visible(false, false, true));
    }
    use super::{
        double_speed_target, one_to_one_window_size, screenshot_directory, status_bar_visible,
    };
    use kattri_client_core::controls::PlaybackSpeed;

    #[test]
    fn one_to_one_window_size_keeps_the_frame_pixels_in_a_16_9_client() {
        assert_eq!(one_to_one_window_size((1920, 1080)), (1920, 1080));
        assert_eq!(one_to_one_window_size((640, 360)), (640, 360));
        // A wide film gets a taller 16:9 client so its pixels are not scaled down.
        assert_eq!(one_to_one_window_size((1920, 800)), (1920, 1080));
        // A 4:3 frame gets a wider client for the same reason.
        assert_eq!(one_to_one_window_size((1440, 1080)), (1920, 1080));
    }

    #[test]
    fn ctrl_2_returns_to_normal_only_from_double_speed() {
        assert_eq!(double_speed_target(Some(2.0)), PlaybackSpeed::Normal);
        assert_eq!(double_speed_target(Some(1.0)), PlaybackSpeed::Double);
        assert_eq!(double_speed_target(Some(1.5)), PlaybackSpeed::Double);
        assert_eq!(double_speed_target(None), PlaybackSpeed::Double);
    }

    #[test]
    fn the_status_bar_hides_while_browsing_with_a_working_player() {
        assert!(!status_bar_visible(false, true, false));
        assert!(status_bar_visible(true, true, false));
        assert!(status_bar_visible(false, false, false));
        assert!(status_bar_visible(false, true, true));
    }

    #[test]
    fn screenshot_folder_must_be_absolute_and_empty_means_the_pictures_folder() {
        assert!(screenshot_directory("Shots").is_err());
        assert!(screenshot_directory("..\\Shots").is_err());
        let absolute = std::env::temp_dir();
        let text = absolute.to_string_lossy().into_owned();
        assert_eq!(screenshot_directory(&text), Ok(absolute));
        if let Ok(pictures) = screenshot_directory("") {
            assert!(pictures.ends_with("Pictures"));
        }
    }
}

impl Drop for GlHost<'_> {
    fn drop(&mut self) {
        let _ = self.destroy();
        if self.mpv.has_pending_reports() {
            // Forced window/OS teardown cannot keep a visible retry UI alive.
            // Log counts only; report bodies contain private media identifiers.
            report_warning(format!(
                "native playback reports not admitted before forced teardown: {:?}",
                self.mpv.pending_report_counts(),
            ));
        }
    }
}

fn physical_video_size(
    points: egui::Vec2,
    pixels_per_point: f32,
    max_texture_size: u32,
) -> [u32; 2] {
    let max_size = max_texture_size.max(1).min(i32::MAX as u32) as f32;
    [points.x, points.y]
        .map(|points| (points * pixels_per_point).round().clamp(1.0, max_size) as u32)
}

fn nonzero_size(size: PhysicalSize<u32>) -> (NonZeroU32, NonZeroU32) {
    (
        NonZeroU32::new(size.width).unwrap_or(NonZeroU32::MIN),
        NonZeroU32::new(size.height).unwrap_or(NonZeroU32::MIN),
    )
}

struct MpvRuntime {
    _library: MpvLibrary,
    session: MpvSession,
}

impl MpvRuntime {
    fn load() -> Result<Self, String> {
        let library = MpvLibrary::load().map_err(|error| error.to_string())?;
        let session = MpvSession::new(&library).map_err(|error| error.to_string())?;
        Ok(Self {
            _library: library,
            session,
        })
    }
}

struct HostApp<'session> {
    host: Option<GlHost<'session>>,
    session: Option<&'session MpvSession>,
    runtime_error: Option<String>,
    local_media: Result<Option<String>, String>,
    profile: Option<String>,
    launch_intent: Option<DeepLinkIntent>,
    #[cfg(target_os = "macos")]
    mac_link_gate: macos_links::MacLinkGate,
    proxy: EventLoopProxy<HostEvent>,
    redraw_at: Option<Instant>,
    failed: bool,
    /// Why startup failed, so `main` can exit non-zero after the event loop stops.
    failure_message: Option<String>,
    /// Discord presence (M4-09). Built only when `plugins.discord_presence` is on and a
    /// Discord application id is configured. Neither is in place yet, so this stays `None`.
    presence: Option<PresenceBinding<IpcTransport>>,
    /// Monotonic origin for the presence clock, which the binding's `shutdown` takes.
    started: Instant,
}

impl<'session> HostApp<'session> {
    fn new(
        session: Option<&'session MpvSession>,
        runtime_error: Option<String>,
        local_media: Result<Option<String>, String>,
        profile: Option<String>,
        launch_intent: Option<DeepLinkIntent>,
        #[cfg(target_os = "macos")] mac_link_gate: macos_links::MacLinkGate,
        proxy: EventLoopProxy<HostEvent>,
    ) -> Self {
        Self {
            host: None,
            session,
            runtime_error,
            local_media,
            profile,
            launch_intent,
            #[cfg(target_os = "macos")]
            mac_link_gate,
            proxy,
            redraw_at: None,
            failed: false,
            failure_message: None,
            presence: None,
            started: Instant::now(),
        }
    }
}

impl ApplicationHandler<HostEvent> for HostApp<'_> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.host.is_some() || self.failed {
            return;
        }
        // Convert any panic during window/GL/libmpv initialization into the normal failure path,
        // and make the process exit non-zero so automation does not read a dead window as success.
        let created = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            GlHost::new(
                event_loop,
                self.proxy.clone(),
                self.session,
                self.runtime_error.clone(),
                self.local_media.clone(),
                self.profile.as_deref(),
                self.launch_intent.take(),
            )
        }));
        match created {
            Ok(Ok(host)) => {
                #[cfg(windows)]
                global_shortcuts::install(self.proxy.clone(), host.global_shortcuts_enabled());
                host.window.request_redraw();
                #[cfg(target_os = "macos")]
                self.mac_link_gate.window_started();
                self.host = Some(host);
            }
            Ok(Err(error)) => {
                report_error(format!("failed to initialize native GL host: {error}"));
                self.failed = true;
                self.failure_message = Some(error.to_string());
                event_loop.exit();
            }
            Err(_) => {
                report_error("the native GL host panicked while starting");
                self.failed = true;
                self.failure_message =
                    Some("the native GL host panicked while starting".to_owned());
                event_loop.exit();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(host) = self.host.as_mut() else {
            return;
        };
        if window_id != host.window.id() {
            return;
        }
        let event_response = host.egui_winit.on_window_event(&host.window, &event);

        match event {
            WindowEvent::CloseRequested => {
                host.save_window_geometry(event_loop);
                host.begin_close();
                if host.advance_close() {
                    event_loop.exit();
                } else {
                    host.window.request_redraw();
                }
            }
            WindowEvent::Destroyed => {
                if !host.destroy() {
                    report_error("window was destroyed before libmpv/GL teardown completed");
                }
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                host.resize(size);
                host.touch_window_geometry();
                host.window.request_redraw();
            }
            WindowEvent::Moved(_) => host.touch_window_geometry(),
            WindowEvent::KeyboardInput {
                event: key_event, ..
            } => {
                if key_event.state == winit::event::ElementState::Pressed
                    && !key_event.repeat
                    && let Some(playing_item_id) = host.mpv.playing_item_id().map(str::to_owned)
                {
                    let handled = handle_winit_media_key(host, &key_event, &playing_item_id);
                    if handled || event_response.repaint {
                        host.window.request_redraw();
                    }
                } else if event_response.repaint {
                    host.window.request_redraw();
                }
            }
            WindowEvent::Focused(focused) => {
                // A defocused window must never keep a hidden cursor.
                host.window_focused = focused;
                host.cursor_idle_since = Instant::now();
                host.show_cursor();
                #[cfg(windows)]
                host.on_window_focus_changed(focused, Instant::now());
            }
            WindowEvent::RedrawRequested => {
                let delay = host.render(event_loop);
                self.redraw_at = if delay.is_zero() {
                    host.window.request_redraw();
                    None
                } else if delay == Duration::MAX {
                    None
                } else {
                    Instant::now().checked_add(delay)
                };
            }
            _ if event_response.repaint => host.window.request_redraw(),
            _ => {}
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: HostEvent) {
        #[cfg(target_os = "macos")]
        let event = match event {
            HostEvent::MacOpenUrl(intent) => {
                if let Some(host) = self.host.as_mut() {
                    host.kattri_app.apply_launch_intent(intent);
                    host.window.request_redraw();
                    self.mac_link_gate.event_applied_to_window();
                } else if macos_links::defer_startup_intent(
                    &mut self.launch_intent,
                    intent,
                    &self.mac_link_gate,
                )
                .is_err()
                {
                    report_warning("rejected macOS Jellyfin URL while startup intent is pending");
                }
                return;
            }
            event => event,
        };

        let Some(host) = self.host.as_mut() else {
            return;
        };
        match event {
            HostEvent::MpvRepaint => {
                host.window.request_redraw();
                self.redraw_at = None;
            }
            HostEvent::EguiRepaint(delay) if delay.is_zero() => {
                host.window.request_redraw();
                self.redraw_at = None;
            }
            HostEvent::EguiRepaint(delay) => {
                let deadline = Instant::now().checked_add(delay);
                self.redraw_at = match (self.redraw_at, deadline) {
                    (Some(current), Some(next)) => Some(current.min(next)),
                    (None, next) => next,
                    (current, None) => current,
                };
            }
            HostEvent::AccessKit(event) if event.window_id == host.window.id() => {
                use egui_winit::accesskit_winit::WindowEvent as AccessKitWindowEvent;
                match event.window_event {
                    AccessKitWindowEvent::InitialTreeRequested => {
                        host.egui.enable_accesskit();
                        host.window.request_redraw();
                    }
                    AccessKitWindowEvent::ActionRequested(request) => {
                        host.egui_winit.on_accesskit_action_request(request);
                        host.window.request_redraw();
                    }
                    AccessKitWindowEvent::AccessibilityDeactivated => {
                        host.egui.disable_accesskit();
                    }
                }
            }
            HostEvent::AccessKit(_) => {}
            #[cfg(windows)]
            HostEvent::GlobalShortcut(action) => {
                if let Some(host) = self.host.as_mut() {
                    match action {
                        global_shortcuts::GlobalAction::TogglePause => host.mpv.toggle_paused(),
                        global_shortcuts::GlobalAction::SeekBackward => host.mpv.seek_relative(-10),
                        global_shortcuts::GlobalAction::SeekForward => host.mpv.seek_relative(10),
                        global_shortcuts::GlobalAction::ToggleMute => host.mpv.toggle_mute(),
                    }
                    host.window.request_redraw();
                }
            }
            #[cfg(target_os = "macos")]
            HostEvent::MacOpenUrl(_) => {}
        }
    }

    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
        if matches!(cause, StartCause::ResumeTimeReached { .. }) {
            if let Some(host) = self.host.as_ref() {
                host.window.request_redraw();
            }
            self.redraw_at = None;
        }
        let _ = event_loop;
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(host) = self.host.as_mut()
            && let Some(identity) = host.mpv.take_runtime_identity()
        {
            host.kattri_app.set_runtime_about(RuntimeAbout {
                mpv_version: (!identity.mpv_version.trim().is_empty())
                    .then(|| identity.mpv_version.clone()),
                ffmpeg_version: identity.ffmpeg_version.clone(),
                gpu: identity.gpu.clone(),
                driver: identity.driver.clone(),
            });
        }
        if let Some(host) = self.host.as_mut() {
            host.update_sleep_inhibit();
            host.poll_screenshot_result();
            if host.advance_close() {
                event_loop.exit();
                return;
            }
            host.save_window_geometry_if_due(event_loop, Instant::now());
            #[cfg(windows)]
            host.minimize_on_defocus_if_due(Instant::now());
            if let Some(delay) = host.report_retry_delay() {
                let deadline = Instant::now() + delay;
                self.redraw_at = Some(
                    self.redraw_at
                        .map_or(deadline, |current| current.min(deadline)),
                );
            }
            if let Some(deadline) = host.window_geometry_deadline() {
                self.redraw_at = Some(
                    self.redraw_at
                        .map_or(deadline, |current| current.min(deadline)),
                );
            }
            if let Some(deadline) = host.cursor_hide_deadline() {
                self.redraw_at = Some(
                    self.redraw_at
                        .map_or(deadline, |current| current.min(deadline)),
                );
            }
            if let Some(deadline) = host.power_poll_deadline() {
                self.redraw_at = Some(
                    self.redraw_at
                        .map_or(deadline, |current| current.min(deadline)),
                );
            }
            if let Some(deadline) = host.sync_play_bridge.retry_at() {
                self.redraw_at = Some(
                    self.redraw_at
                        .map_or(deadline, |current| current.min(deadline)),
                );
            }
            #[cfg(windows)]
            if let Some(deadline) = host.defocus_minimizer.deadline() {
                self.redraw_at = Some(
                    self.redraw_at
                        .map_or(deadline, |current| current.min(deadline)),
                );
            }
            #[cfg(debug_assertions)]
            if let Some(deadline) = debug_harness::poll_deadline() {
                self.redraw_at = Some(
                    self.redraw_at
                        .map_or(deadline, |current| current.min(deadline)),
                );
            }
            #[cfg(debug_assertions)]
            if let Some(deadline) = position_trace::poll_deadline(Instant::now()) {
                self.redraw_at = Some(
                    self.redraw_at
                        .map_or(deadline, |current| current.min(deadline)),
                );
            }
        }
        event_loop.set_control_flow(match self.redraw_at {
            Some(deadline) => ControlFlow::WaitUntil(deadline),
            None => ControlFlow::Wait,
        });
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(host) = self.host.as_mut()
            && !host.destroy()
        {
            report_error("libmpv renderer could not be closed before host teardown");
        }
        // Clears the Discord activity and closes the IPC connection. Without this, a
        // presence shown at exit stays on the Discord profile until Discord times it out.
        if let Some(presence) = self.presence.as_mut() {
            presence.shutdown(self.started.elapsed());
        }
    }
}

/// The fullscreen cursor hides only when the window is a focused, fullscreen surface, the pointer
/// has been still long enough, and this frame carried no input. Input always brings it back.
fn cursor_should_hide(fullscreen: bool, focused: bool, idle: Duration, input_seen: bool) -> bool {
    fullscreen && focused && !input_seen && idle >= CURSOR_HIDE_AFTER
}

fn has_user_activity(events: &[egui::Event]) -> bool {
    events.iter().any(|event| match event {
        egui::Event::PointerMoved(_)
        | egui::Event::MouseMoved(_)
        | egui::Event::MouseWheel { .. }
        | egui::Event::Zoom(_)
        | egui::Event::Rotate(_)
        | egui::Event::Touch { .. }
        | egui::Event::Text(_)
        | egui::Event::Copy
        | egui::Event::Cut
        | egui::Event::Paste(_) => true,
        egui::Event::PointerButton { pressed, .. } => *pressed,
        egui::Event::Key { pressed, .. } => *pressed,
        _ => false,
    })
}

fn duration_from_seconds(seconds: f64) -> Option<Duration> {
    (seconds.is_finite() && seconds >= 0.0)
        .then(|| Duration::try_from_secs_f64(seconds).ok())
        .flatten()
}

/// Starts the production log file and the panic log. A failure (no data folder, read-only disk,
/// a subscriber already installed) leaves the app running without a log file.
fn init_logging() -> Option<kattri_logging::LogGuard> {
    let dir = kattri_logging::log_directory()?;
    let level = kattri_logging::resolve_level(
        None,
        std::env::var(kattri_logging::LEVEL_ENV_VAR).ok().as_deref(),
    );
    let guard =
        kattri_logging::init(&kattri_logging::LogConfig::new(dir).with_level(level)).ok()?;
    kattri_logging::install_panic_hook();
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        "Jellyfin Kattri started"
    );
    Some(guard)
}

/// True when `KATTRI_LOG` names a level: it then wins over the Settings choice for this run.
fn log_level_set_by_environment() -> bool {
    static PINNED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *PINNED.get_or_init(|| {
        std::env::var(kattri_logging::LEVEL_ENV_VAR)
            .ok()
            .is_some_and(|text| text.parse::<kattri_logging::LogLevel>().is_ok())
    })
}

/// Makes the running log follow the Settings choice. Cheap when nothing changed.
fn apply_log_detail(detail: kattri_settings::LogDetail) {
    use kattri_logging::LogLevel;
    use kattri_settings::LogDetail;
    if log_level_set_by_environment() {
        return;
    }
    let wanted = match detail {
        LogDetail::Error => LogLevel::Error,
        LogDetail::Warn => LogLevel::Warn,
        LogDetail::Info => LogLevel::Info,
        LogDetail::Debug => LogLevel::Debug,
    };
    if kattri_logging::current_level().is_some_and(|now| now != wanted) {
        kattri_logging::set_level(wanted);
    }
}

/// CI-only launch check: create the real window and exit without OpenGL. The GPU-less Windows
/// runners cannot create a WGL context, but they can still prove the installed binary starts,
/// shows a titled window and closes normally.
fn run_window_smoke() -> Result<(), Box<dyn Error>> {
    struct Smoke {
        window: Option<Window>,
        deadline: Instant,
    }

    impl ApplicationHandler for Smoke {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            if self.window.is_some() {
                return;
            }
            let attributes = Window::default_attributes()
                .with_title("Jellyfin Kattri")
                .with_inner_size(LogicalSize::new(640.0, 360.0))
                .with_visible(true);
            match event_loop.create_window(attributes) {
                Ok(window) => self.window = Some(window),
                Err(_) => event_loop.exit(),
            }
        }

        fn window_event(
            &mut self,
            event_loop: &ActiveEventLoop,
            _id: WindowId,
            event: WindowEvent,
        ) {
            if matches!(event, WindowEvent::CloseRequested | WindowEvent::Destroyed) {
                event_loop.exit();
            }
        }

        fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
            if Instant::now() >= self.deadline {
                event_loop.exit();
            } else {
                event_loop.set_control_flow(ControlFlow::WaitUntil(self.deadline));
            }
        }
    }

    let event_loop = EventLoop::<()>::with_user_event().build()?;
    let mut smoke = Smoke {
        window: None,
        deadline: Instant::now() + Duration::from_secs(8),
    };
    event_loop.run_app(&mut smoke)?;
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    #[cfg(debug_assertions)]
    position_trace::configure(Instant::now(), debug_syncplay_status_opt_in());
    #[cfg(debug_assertions)]
    qa_metrics::start(Instant::now());
    let _log_guard = init_logging();
    if std::env::args_os().any(|argument| argument == "--window-smoke") {
        return run_window_smoke();
    }
    // A bad command line is reported through the same status line as a bad media path.
    let (profile, local_media, launch_intent) = match parse_host_args(std::env::args_os().skip(1)) {
        Ok(args) => (
            args.profile,
            local_media_path(args.media),
            args.launch_intent,
        ),
        Err(error) => (None, Err(error.to_string()), None),
    };
    let event_loop = EventLoop::<HostEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    #[cfg(target_os = "macos")]
    let mac_link_gate = macos_links::MacLinkGate::new(launch_intent.is_some());
    #[cfg(target_os = "macos")]
    let _macos_url_registration = match macos_links::register(proxy.clone(), mac_link_gate.clone())
    {
        Ok(registration) => Some(registration),
        Err(error) => {
            report_warning(format!("macOS Jellyfin URL dispatch unavailable: {error}"));
            None
        }
    };
    let runtime = MpvRuntime::load();
    let runtime_error = runtime.as_ref().err().cloned();
    let runtime = runtime.ok();
    let session = runtime.as_ref().map(|runtime| &runtime.session);
    let mut app = HostApp::new(
        session,
        runtime_error,
        local_media,
        profile,
        launch_intent,
        #[cfg(target_os = "macos")]
        mac_link_gate,
        proxy,
    );
    event_loop.run_app(&mut app)?;
    // A failed window/GL setup previously exited 0, which made installers and CI treat a dead
    // app as a successful launch. Report it as an error instead.
    if let Some(message) = app.failure_message.take() {
        return Err(message.into());
    }
    Ok(())
}

/// Only the explicit debug-runner opt-in disables reporting; release builds ignore it.
#[cfg(any(debug_assertions, test))]
fn stream_only_validation(value: Option<&OsStr>) -> bool {
    value == Some(OsStr::new("1"))
}

#[cfg(debug_assertions)]
fn debug_syncplay_status_opt_in() -> bool {
    debug_validation::stream_only_syncplay_opt_in(
        stream_only_validation(std::env::var_os("KATTRI_DEBUG_STREAM_ONLY").as_deref()),
        stream_only_validation(std::env::var_os("KATTRI_DEBUG_SYNCPLAY_STATUS").as_deref()),
    )
}

#[cfg(not(debug_assertions))]
fn debug_syncplay_status_opt_in() -> bool {
    false
}

fn should_resume_saved_sign_in(
    local_media: &Result<Option<String>, String>,
    has_launch_intent: bool,
) -> bool {
    !has_launch_intent && matches!(local_media, Ok(None))
}

#[cfg(test)]
mod tests {
    use super::{
        CURSOR_HIDE_AFTER, HostShortcutAction, TheaterEscapeAction,
        append_gamepad_navigation_events, cursor_should_hide, duration_from_seconds,
        has_user_activity, host_shortcut_action, label_video_surface, playback_shortcuts_enabled,
        player_surface_active, should_resume_saved_sign_in, should_use_theater_view,
        theater_escape_action, theater_skeleton_visible, video_click_toggles,
    };
    use egui::{Event, Key, Modifiers};
    use std::time::Duration;

    #[test]
    fn theater_escape_closes_dialog_popup_then_dialog_before_player_popup() {
        let context = egui::Context::default();
        egui::Popup::open_id(&context, egui::Id::unique("subtitle-combobox"));

        assert_eq!(
            theater_escape_action(&context, true, true, true, true),
            TheaterEscapeAction::ClosePopup
        );
        assert!(!egui::Popup::is_any_open(&context));
        assert_eq!(
            theater_escape_action(&context, true, true, true, true),
            TheaterEscapeAction::CloseSubtitleSearch
        );
        egui::Popup::open_id(&context, egui::Id::unique("player-settings-popup"));
        assert_eq!(
            theater_escape_action(&context, true, false, false, true),
            TheaterEscapeAction::ClosePopup
        );
        assert!(!egui::Popup::is_any_open(&context));
        assert_eq!(
            theater_escape_action(&context, true, false, false, true),
            TheaterEscapeAction::CloseMediaInfo
        );
        assert_eq!(
            theater_escape_action(&context, true, false, false, false),
            TheaterEscapeAction::StopPlayback
        );
    }

    #[test]
    fn theater_escape_does_not_close_popup_while_typing_or_without_escape() {
        let context = egui::Context::default();
        egui::Popup::open_id(&context, egui::Id::unique("player-settings-popup"));

        assert_eq!(
            theater_escape_action(&context, false, false, false, false),
            TheaterEscapeAction::None
        );
        assert_eq!(
            theater_escape_action(&context, true, true, true, false),
            TheaterEscapeAction::ClosePopup
        );
        assert!(!egui::Popup::is_any_open(&context));
        assert_eq!(
            theater_escape_action(&context, true, true, false, false),
            TheaterEscapeAction::None
        );
    }

    #[test]
    fn theater_escape_does_not_interrupt_typing_outside_subtitle_dialog() {
        let context = egui::Context::default();
        assert_eq!(
            theater_escape_action(&context, true, true, false, true),
            TheaterEscapeAction::None
        );
    }

    #[test]
    fn playback_shortcuts_are_gated_by_typing_and_egui_popups() {
        assert!(playback_shortcuts_enabled(false, false));
        assert!(!playback_shortcuts_enabled(true, false));
        assert!(!playback_shortcuts_enabled(false, true));
        assert!(!playback_shortcuts_enabled(true, true));
    }

    #[test]
    fn host_shortcuts_match_fullscreen_minimize_and_always_on_top() {
        let key = |key, pressed, repeat, modifiers| Event::Key {
            key,
            physical_key: Some(key),
            pressed,
            repeat,
            modifiers,
        };
        assert_eq!(
            host_shortcut_action(&[key(Key::F11, true, false, egui::Modifiers::NONE)]),
            Some(HostShortcutAction::ToggleFullscreen)
        );
        assert_eq!(
            host_shortcut_action(&[key(Key::Enter, true, false, egui::Modifiers::ALT)]),
            Some(HostShortcutAction::ToggleFullscreen)
        );
        assert_eq!(
            host_shortcut_action(&[key(Key::M, true, false, egui::Modifiers::CTRL)]),
            Some(HostShortcutAction::Minimize)
        );
        assert_eq!(
            host_shortcut_action(&[key(
                Key::A,
                true,
                false,
                egui::Modifiers::CTRL | egui::Modifiers::SHIFT
            )]),
            Some(HostShortcutAction::ToggleAlwaysOnTop)
        );
        assert_eq!(
            host_shortcut_action(&[key(Key::A, true, false, egui::Modifiers::CTRL)]),
            None,
            "Ctrl+A belongs to aspect cycle, not always-on-top"
        );
        assert_eq!(
            host_shortcut_action(&[key(Key::F11, true, true, egui::Modifiers::NONE)]),
            None,
            "repeat"
        );
    }

    #[test]
    fn gamepad_navigation_uses_the_egui_focus_path() {
        use kattri_client_core::focus::NavAction;

        let mut first_direction = egui::RawInput::default();
        assert!(!append_gamepad_navigation_events(
            &mut first_direction,
            &[NavAction::Down],
            false,
        ));
        assert!(matches!(
            first_direction.events.as_slice(),
            [Event::Key {
                key: Key::Tab,
                pressed: true,
                ..
            }]
        ));

        let mut focused_direction = egui::RawInput::default();
        assert!(!append_gamepad_navigation_events(
            &mut focused_direction,
            &[NavAction::Right],
            true,
        ));
        assert!(matches!(
            focused_direction.events.as_slice(),
            [Event::Key {
                key: Key::ArrowRight,
                pressed: true,
                ..
            }]
        ));

        let mut focused_select = egui::RawInput::default();
        assert!(!append_gamepad_navigation_events(
            &mut focused_select,
            &[NavAction::Select],
            true,
        ));
        assert!(matches!(
            focused_select.events.as_slice(),
            [Event::Key {
                key: Key::Enter,
                pressed: true,
                ..
            }]
        ));

        let mut unfocused_select = egui::RawInput::default();
        assert!(append_gamepad_navigation_events(
            &mut unfocused_select,
            &[NavAction::Select],
            false,
        ));
        assert!(unfocused_select.events.is_empty());
    }

    #[test]
    fn video_surface_is_named_and_does_not_take_keyboard_focus() {
        let context = egui::Context::default();
        context.enable_accesskit();
        let mut output = context.run_ui(egui::RawInput::default(), |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                let (_, response) =
                    ui.allocate_exact_size(egui::vec2(320.0, 180.0), egui::Sense::CLICK);
                label_video_surface(&response, Some("Sample film"));
            });
        });
        output.textures_delta.clear();
        let tree = output
            .platform_output
            .accesskit_update
            .expect("AccessKit tree is emitted");
        let node = tree
            .nodes
            .iter()
            .find(|(_, node)| {
                node.role() == egui::accesskit::Role::Image
                    && node.label() == Some("Video playback surface: Sample film")
            })
            .map(|(_, node)| node)
            .expect("the video surface has an accessible name");
        assert!(
            !node.supports_action(egui::accesskit::Action::Focus),
            "the video surface is not a keyboard focus target"
        );
    }

    #[test]
    fn media_info_hover_opens_the_sanitized_details_surface() {
        use super::media_info_panel::{self, MediaInfoState, RuntimeDiagnostics};
        use super::player_controls::{TopChromeAction, TopChromeView, draw_top_chrome_outcome};
        use kattri_client_core::media_info::{MediaInfo, MediaInput};

        let context = egui::Context::default();
        let chrome = TopChromeView {
            title: Some("Sample film"),
            subtitle: None,
            opacity: 1.0,
            media_info_open: false,
            show_media_info: true,
        };
        let mut state = MediaInfoState::default();
        let mut trigger = None;
        let mut init_output = context.run_ui(egui::RawInput::default(), |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                let top = draw_top_chrome_outcome(ui, chrome);
                assert_eq!(top.action, TopChromeAction::None);
                trigger = Some(top.media_info_rect);
            });
        });
        init_output.textures_delta.clear();
        let trigger_pos = trigger.expect("Media info trigger").center();

        let info = MediaInfo::from_input(&MediaInput::default());
        let runtime = RuntimeDiagnostics {
            status: "Synthetic runtime status",
            capability_warning: Some("Synthetic capability warning"),
            report_feedback: None,
        };

        // Hovering the Media info trigger opens the panel and stays open across multiple frames.
        let mut panel_rect = None;
        for _ in 0..3 {
            let mut output = context.run_ui(
                egui::RawInput {
                    events: vec![Event::PointerMoved(trigger_pos)],
                    ..egui::RawInput::default()
                },
                |ui| {
                    egui::CentralPanel::default().show(ui, |ui| {
                        let open_chrome = TopChromeView {
                            media_info_open: state.is_open(),
                            ..chrome
                        };
                        let top = draw_top_chrome_outcome(ui, open_chrome);
                        if top.action == TopChromeAction::MediaInfo {
                            state.toggle();
                        }
                        let mut panel_hot = false;
                        if state.is_open() {
                            let outcome = media_info_panel::show(ui.ctx(), &info, runtime, None);
                            panel_hot = outcome.hovered_or_focused;
                            panel_rect = Some(outcome.rect);
                        }
                        state.set_hover(top.media_info_hot || panel_hot);
                    });
                },
            );
            output.textures_delta.clear();
            assert!(
                state.is_open(),
                "hover keeps media info open without flickering"
            );
        }

        // Moving the pointer into the rendered Media info panel keeps it open across frames.
        let panel_center = panel_rect.expect("media info panel rect").center();
        for _ in 0..2 {
            let mut output = context.run_ui(
                egui::RawInput {
                    events: vec![Event::PointerMoved(panel_center)],
                    ..egui::RawInput::default()
                },
                |ui| {
                    egui::CentralPanel::default().show(ui, |ui| {
                        let open_chrome = TopChromeView {
                            media_info_open: state.is_open(),
                            ..chrome
                        };
                        let top = draw_top_chrome_outcome(ui, open_chrome);
                        let outcome = media_info_panel::show(ui.ctx(), &info, runtime, None);
                        assert!(outcome.hovered_or_focused);
                        state.set_hover(top.media_info_hot || outcome.hovered_or_focused);
                    });
                },
            );
            output.textures_delta.clear();
            let painted = output
                .shapes
                .iter()
                .filter_map(|clipped| match &clipped.shape {
                    egui::Shape::Text(text) => Some(text.galley.text()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            for expected in [
                "Media info",
                "Copy diagnostics",
                "Close",
                "Container",
                "Unavailable",
                "Synthetic runtime status",
                "Synthetic capability warning",
            ] {
                assert!(
                    painted.iter().any(|text| text.contains(expected)),
                    "hover-opened details show {expected:?}"
                );
            }
            assert!(
                state.is_open(),
                "panel stays open while pointer is inside panel"
            );
        }

        // Moving pointer away when unpinned closes the panel cleanly.
        let mut output = context.run_ui(
            egui::RawInput {
                events: vec![Event::PointerMoved(egui::pos2(8.0, 8.0))],
                ..egui::RawInput::default()
            },
            |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    let open_chrome = TopChromeView {
                        media_info_open: state.is_open(),
                        ..chrome
                    };
                    let top = draw_top_chrome_outcome(ui, open_chrome);
                    let outcome = media_info_panel::show(ui.ctx(), &info, runtime, None);
                    state.set_hover(top.media_info_hot || outcome.hovered_or_focused);
                });
            },
        );
        output.textures_delta.clear();
        assert!(
            !state.is_open(),
            "moving pointer away unpinned closes panel"
        );
    }

    fn click_events(position: egui::Pos2) -> Vec<Event> {
        vec![
            Event::PointerMoved(position),
            Event::PointerButton {
                pos: position,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: position,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ]
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum HitOverlay {
        None,
        CenterPlay,
        Toolbar,
        Seek,
    }

    struct HitFrame {
        raw_video_clicked: bool,
        video_clicked: bool,
        overlay_clicked: bool,
        seek_dragged: bool,
        popup_click_blocked: bool,
        media_info_click_blocked: bool,
        subtitle_search_click_blocked: bool,
        center_play_click_blocked: bool,
    }

    fn hit_frame(
        context: &egui::Context,
        mut input: egui::RawInput,
        overlay: HitOverlay,
    ) -> HitFrame {
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(640.0, 360.0));
        input.screen_rect.get_or_insert(screen);
        let mut result = HitFrame {
            raw_video_clicked: false,
            video_clicked: false,
            overlay_clicked: false,
            seek_dragged: false,
            popup_click_blocked: false,
            media_info_click_blocked: false,
            subtitle_search_click_blocked: false,
            center_play_click_blocked: false,
        };
        let mut output = context.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                let video = ui.interact(
                    screen,
                    ui.make_persistent_id("test-video-surface"),
                    egui::Sense::CLICK,
                );
                let center_play_clicked = if overlay == HitOverlay::CenterPlay {
                    egui::Area::new(egui::Id::unique("test-center-play"))
                        .order(egui::Order::Foreground)
                        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                        .show(ui.ctx(), |ui| {
                            let (_, response) = ui
                                .allocate_exact_size(egui::vec2(84.0, 84.0), egui::Sense::click());
                            response.clicked()
                        })
                        .inner
                } else {
                    false
                };
                let overlay_clicked = match overlay {
                    HitOverlay::Toolbar => {
                        egui::Area::new(egui::Id::unique("test-toolbar"))
                            .order(egui::Order::Foreground)
                            .anchor(egui::Align2::LEFT_TOP, egui::Vec2::ZERO)
                            .show(ui.ctx(), |ui| ui.button("Toolbar").clicked())
                            .inner
                    }
                    HitOverlay::Seek => {
                        egui::Area::new(egui::Id::unique("test-seek"))
                            .order(egui::Order::Foreground)
                            .anchor(egui::Align2::LEFT_BOTTOM, egui::Vec2::ZERO)
                            .show(ui.ctx(), |ui| {
                                let (_, response) = ui.allocate_exact_size(
                                    egui::vec2(640.0, 26.0),
                                    egui::Sense::click_and_drag(),
                                );
                                result.seek_dragged = response.dragged();
                                response.clicked()
                            })
                            .inner
                    }
                    HitOverlay::CenterPlay => center_play_clicked,
                    HitOverlay::None => false,
                };
                result.raw_video_clicked = video.clicked_by(egui::PointerButton::Primary);
                result.video_clicked = video_click_toggles(
                    &video,
                    center_play_clicked,
                    egui::Popup::is_any_open(ui.ctx()),
                    false,
                    false,
                );
                result.popup_click_blocked =
                    !video_click_toggles(&video, false, true, false, false);
                result.media_info_click_blocked =
                    !video_click_toggles(&video, false, false, true, false);
                result.subtitle_search_click_blocked =
                    !video_click_toggles(&video, false, false, false, true);
                result.center_play_click_blocked =
                    !video_click_toggles(&video, true, false, false, false);
                result.overlay_clicked = overlay_clicked;
            });
        });
        output.textures_delta.clear();
        result
    }

    #[test]
    fn one_video_click_toggles_once_and_overlay_clicks_do_not_propagate() {
        let center = egui::pos2(320.0, 180.0);
        let context = egui::Context::default();
        hit_frame(&context, egui::RawInput::default(), HitOverlay::None);
        let clicked = hit_frame(
            &context,
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(640.0, 360.0),
                )),
                events: click_events(center),
                ..egui::RawInput::default()
            },
            HitOverlay::None,
        );
        assert!(clicked.raw_video_clicked);
        assert!(clicked.video_clicked);
        assert_eq!(usize::from(clicked.video_clicked), 1);

        for overlay in [HitOverlay::CenterPlay, HitOverlay::Toolbar] {
            let context = egui::Context::default();
            for _ in 0..2 {
                hit_frame(
                    &context,
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(640.0, 360.0),
                        )),
                        ..egui::RawInput::default()
                    },
                    overlay,
                );
            }
            let position = if overlay == HitOverlay::CenterPlay {
                center
            } else {
                egui::pos2(40.0, 20.0)
            };
            let mut events = vec![Event::PointerMoved(position)];
            events.extend(click_events(position));
            let frame = hit_frame(
                &context,
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(640.0, 360.0),
                    )),
                    events,
                    ..egui::RawInput::default()
                },
                overlay,
            );
            assert!(frame.overlay_clicked, "{overlay:?} owns the click");
            assert!(!frame.video_clicked, "{overlay:?} must not toggle playback");
        }
    }

    #[test]
    fn seek_drag_and_popup_dismiss_click_do_not_toggle_playback() {
        let context = egui::Context::default();
        for _ in 0..2 {
            hit_frame(
                &context,
                egui::RawInput {
                    ..egui::RawInput::default()
                },
                HitOverlay::Seek,
            );
        }
        hit_frame(
            &context,
            egui::RawInput {
                events: vec![
                    Event::PointerMoved(egui::pos2(320.0, 347.0)),
                    Event::PointerButton {
                        pos: egui::pos2(320.0, 347.0),
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: Modifiers::NONE,
                    },
                ],
                ..egui::RawInput::default()
            },
            HitOverlay::Seek,
        );
        let dragged = hit_frame(
            &context,
            egui::RawInput {
                events: vec![Event::PointerMoved(egui::pos2(420.0, 347.0))],
                ..egui::RawInput::default()
            },
            HitOverlay::Seek,
        );
        assert!(dragged.seek_dragged);
        assert!(!dragged.video_clicked);
        hit_frame(
            &context,
            egui::RawInput {
                events: vec![Event::PointerButton {
                    pos: egui::pos2(420.0, 347.0),
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: Modifiers::NONE,
                }],
                ..egui::RawInput::default()
            },
            HitOverlay::Seek,
        );

        let context = egui::Context::default();
        hit_frame(&context, egui::RawInput::default(), HitOverlay::None);
        let popup_id = egui::Id::unique("test-player-popup");
        egui::Popup::open_id(&context, popup_id);
        let dismissed = hit_frame(
            &context,
            egui::RawInput {
                events: click_events(egui::pos2(320.0, 180.0)),
                ..egui::RawInput::default()
            },
            HitOverlay::None,
        );
        assert!(
            dismissed.raw_video_clicked,
            "the outside click reaches the video"
        );
        assert!(
            dismissed.popup_click_blocked,
            "the open-popup guard blocks playback"
        );
        assert!(!dismissed.video_clicked);
    }

    #[test]
    fn popup_and_media_info_guards_suppress_a_raw_video_click() {
        let context = egui::Context::default();
        hit_frame(&context, egui::RawInput::default(), HitOverlay::None);
        let raw = hit_frame(
            &context,
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(640.0, 360.0),
                )),
                events: click_events(egui::pos2(320.0, 180.0)),
                ..egui::RawInput::default()
            },
            HitOverlay::None,
        );
        assert!(raw.raw_video_clicked);
        assert!(raw.video_clicked);
        assert!(raw.popup_click_blocked);
        assert!(raw.media_info_click_blocked);
        assert!(raw.subtitle_search_click_blocked);
        assert!(raw.center_play_click_blocked);
    }

    #[test]
    fn local_media_and_deep_link_launches_skip_saved_server_resume() {
        let local_file = Ok(Some("synthetic-video.mp4".to_owned()));
        let invalid_file = Err("local media is missing".to_owned());
        let no_file = Ok(None);

        assert!(!should_resume_saved_sign_in(&local_file, false));
        assert!(!should_resume_saved_sign_in(&invalid_file, false));
        assert!(!should_resume_saved_sign_in(&no_file, true));
        assert!(should_resume_saved_sign_in(&no_file, false));
    }

    #[test]
    fn active_media_uses_the_theater_player_surface() {
        assert!(should_use_theater_view(true, true));
        assert!(!should_use_theater_view(true, false));
        assert!(!should_use_theater_view(false, true));
    }

    #[test]
    fn playback_preparation_activates_skeleton_theater_until_media_or_pip_takes_over() {
        let surface = player_surface_active(false, true, false, false);
        assert!(should_use_theater_view(true, surface));
        assert!(!should_use_theater_view(false, surface));
        assert!(theater_skeleton_visible(true, false));

        // While libmpv is loading (StartFile -> FileLoaded -> first frame), theater and skeleton stay active.
        assert!(should_use_theater_view(
            true,
            player_surface_active(true, false, true, false)
        ));
        assert!(theater_skeleton_visible(false, true));

        // Once libmpv finishes loading and renders the first frame, the skeleton gives way to the live player.
        assert!(should_use_theater_view(
            true,
            player_surface_active(true, false, false, false)
        ));
        assert!(!theater_skeleton_visible(false, false));

        // PiP keeps the theater surface active so video continues rendering inside the PiP window.
        assert!(should_use_theater_view(
            true,
            player_surface_active(true, false, false, true)
        ));
        assert!(should_use_theater_view(
            true,
            player_surface_active(false, true, false, true)
        ));
    }

    #[test]
    fn cursor_hides_only_in_a_focused_fullscreen_window_after_idle() {
        let idle = CURSOR_HIDE_AFTER;
        assert!(cursor_should_hide(true, true, idle, false));
        assert!(cursor_should_hide(
            true,
            true,
            idle + Duration::from_secs(4),
            false
        ));
        assert!(
            !cursor_should_hide(true, true, idle, true),
            "input shows it"
        );
        assert!(!cursor_should_hide(
            true,
            true,
            CURSOR_HIDE_AFTER - Duration::from_millis(1),
            false
        ));
        assert!(!cursor_should_hide(false, true, idle, false), "windowed");
        assert!(!cursor_should_hide(true, false, idle, false), "unfocused");
    }

    #[test]
    fn stream_only_validation_requires_the_exact_debug_opt_in() {
        use super::stream_only_validation;
        use std::ffi::OsStr;
        assert!(stream_only_validation(Some(OsStr::new("1"))));
        for value in [None, Some(""), Some("0"), Some("true"), Some(" 1")] {
            assert!(!stream_only_validation(value.map(OsStr::new)));
        }
    }

    #[test]
    fn pointer_and_pressed_keyboard_events_count_as_osd_activity() {
        assert!(has_user_activity(&[Event::PointerMoved(egui::pos2(
            2.0, 3.0
        ))]));
        assert!(has_user_activity(&[Event::Key {
            key: Key::Space,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }]));
        assert!(!has_user_activity(&[Event::Key {
            key: Key::Space,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers: Modifiers::NONE,
        }]));
    }

    #[test]
    fn playback_property_values_are_safely_converted_for_the_controls() {
        assert_eq!(
            duration_from_seconds(65.5),
            Some(Duration::from_millis(65_500))
        );
        assert_eq!(duration_from_seconds(f64::NAN), None);
        assert_eq!(duration_from_seconds(-1.0), None);
    }
}

#[cfg(test)]
mod up_next_tests {
    use super::{UP_NEXT_LEAD, up_next_due};
    use std::time::Duration;

    #[test]
    fn up_next_appears_in_the_last_half_minute_of_a_long_video_only() {
        let episode = Duration::from_secs(45 * 60);
        assert!(!up_next_due(
            episode - UP_NEXT_LEAD - Duration::from_secs(1),
            episode
        ));
        assert!(up_next_due(episode - UP_NEXT_LEAD, episode));
        assert!(up_next_due(episode, episode));
        let clip = Duration::from_secs(90);
        assert!(!up_next_due(clip, clip), "a short clip has no Up Next card");
    }
}

use std::{
    ffi::{CStr, c_void},
    num::NonZeroU32,
    rc::Rc,
    sync::Arc,
    thread::{self, ThreadId},
};

use glow::HasContext;
use glutin::{
    context::{PossiblyCurrentContext, PossiblyCurrentGlContext},
    display::{Display, GlDisplay},
    surface::{Surface, WindowSurface},
};
use mpv_render_proof::{
    CurrentGlContext, DecodeInteropEvidence, GlContextProvider, GlImplementation, GlStateRestore,
    HostPlatform,
};

pub(crate) struct ContextParts {
    // Keep the native context and surface ahead of the display so Rust drops
    // them before the display. The host window outlives this Rc in GlHost.
    pub(crate) context: PossiblyCurrentContext,
    pub(crate) surface: Surface<WindowSurface>,
    pub(crate) display: Display,
}

impl ContextParts {
    pub(crate) fn make_current(&self) -> Result<(), String> {
        self.context
            .make_current(&self.surface)
            .map_err(|error| error.to_string())
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct GlCapabilities {
    pub(crate) is_gles: bool,
    pub(crate) supports_gl3: bool,
    pub(crate) supports_samplers: bool,
    pub(crate) supports_unpack_extras: bool,
    pub(crate) unit_count: u8,
    pub(crate) max_texture_size: u32,
}

impl GlCapabilities {
    /// Query static OpenGL capabilities once while the host context is current.
    pub(crate) fn query(gl: &glow::Context) -> Self {
        // SAFETY: callers query capabilities only while the host context is
        // current on its owning thread.
        unsafe {
            let version = gl.get_parameter_string(glow::VERSION);
            let is_gles = version.contains("OpenGL ES");
            let supports_gl3 = version_at_least(&version, 3, 0);
            let supports_unpack_extras = !is_gles || supports_gl3;
            let supports_samplers =
                (is_gles && supports_gl3) || (!is_gles && version_at_least(&version, 3, 3));
            let unit_count = gl
                .get_parameter_i32(glow::MAX_COMBINED_TEXTURE_IMAGE_UNITS)
                .clamp(1, 8) as u8;
            let max_texture_size = gl.get_parameter_i32(glow::MAX_TEXTURE_SIZE).max(1) as u32;
            Self {
                is_gles,
                supports_gl3,
                supports_samplers,
                supports_unpack_extras,
                unit_count,
                max_texture_size,
            }
        }
    }
}

pub(crate) struct HostGlContextProvider {
    parts: Rc<ContextParts>,
    gl: Arc<glow::Context>,
    capabilities: GlCapabilities,
    owner: ThreadId,
}

// The render proof requires a Send + Sync provider because libmpv stores its
// proc-address callback. Its callbacks are thread-checked below: context and
// Rc access are only performed on the event-loop thread, while a callback from
// another thread fails closed without touching either non-Send handle.
// MpvRenderer itself is !Send, and the host drops every provider owner on the
// event-loop thread after closing the render context.
unsafe impl Send for HostGlContextProvider {}
unsafe impl Sync for HostGlContextProvider {}

impl HostGlContextProvider {
    pub(crate) fn new(parts: Rc<ContextParts>, gl: Arc<glow::Context>) -> Arc<Self> {
        let capabilities = GlCapabilities::query(&gl);
        Arc::new(Self {
            parts,
            gl,
            capabilities,
            owner: thread::current().id(),
        })
    }

    pub(crate) fn capabilities(&self) -> GlCapabilities {
        self.capabilities
    }

    fn on_owner_thread(&self) -> bool {
        thread::current().id() == self.owner
    }
}

unsafe impl GlContextProvider for HostGlContextProvider {
    fn make_current(&self) -> Result<Box<dyn CurrentGlContext + '_>, String> {
        if !self.on_owner_thread() {
            return Err("libmpv GL access was requested off the window thread".into());
        }
        self.parts.make_current()?;
        if !self.parts.context.is_current() {
            return Err("the host OpenGL context did not become current".into());
        }
        Ok(Box::new(CurrentContext { provider: self }))
    }

    fn get_proc_address(&self, name: &CStr) -> *mut c_void {
        if !self.on_owner_thread() {
            return std::ptr::null_mut();
        }
        self.parts.display.get_proc_address(name) as *mut c_void
    }

    fn interop_evidence(&self) -> DecodeInteropEvidence {
        DecodeInteropEvidence {
            platform: host_platform(),
            gl: GlImplementation::Other,
            linux_vendor: None,
            linux_display: None,
            gl_dx_interop_proven: false,
            vdpau_enabled: false,
            cgl_context_current: false,
        }
    }
}

fn host_platform() -> HostPlatform {
    #[cfg(target_os = "windows")]
    {
        HostPlatform::Windows
    }
    #[cfg(target_os = "macos")]
    {
        HostPlatform::MacOs
    }
    #[cfg(target_os = "linux")]
    {
        HostPlatform::Linux
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        HostPlatform::Other
    }
}

struct CurrentContext<'a> {
    provider: &'a HostGlContextProvider,
}

impl CurrentGlContext for CurrentContext<'_> {
    fn preserve_egui_state(&self) -> Box<dyn GlStateRestore + '_> {
        let snapshot = GlStateSnapshot::capture_with_caps(
            Arc::clone(&self.provider.gl),
            self.provider.capabilities,
        );
        snapshot.reset_for_mpv();
        Box::new(snapshot)
    }
}

#[derive(Clone, Copy)]
struct UnpackExtras {
    row_length: i32,
    skip_rows: i32,
    skip_pixels: i32,
    pixel_unpack_buffer: i32,
}

#[derive(Clone, Copy, Default)]
struct TextureUnitState {
    texture_2d: i32,
    sampler: Option<i32>,
}

pub(crate) struct GlStateSnapshot {
    gl: Arc<glow::Context>,
    framebuffer: i32,
    read_framebuffer: Option<i32>,
    draw_framebuffer: Option<i32>,
    viewport: [i32; 4],
    scissor_box: [i32; 4],
    current_program: i32,
    array_buffer: i32,
    element_array_buffer: i32,
    vertex_array: Option<i32>,
    active_texture: u32,
    texture_units: [TextureUnitState; 8],
    unit_count: u8,
    active_texture_binding: i32,
    active_sampler_binding: Option<i32>,
    unpack_alignment: i32,
    pack_alignment: i32,
    unpack_extras: Option<UnpackExtras>,
    blend_enabled: bool,
    cull_enabled: bool,
    depth_enabled: bool,
    scissor_enabled: bool,
    stencil_enabled: bool,
    framebuffer_srgb: Option<bool>,
    blend_equation_rgb: i32,
    blend_equation_alpha: i32,
    blend_src_rgb: i32,
    blend_dst_rgb: i32,
    blend_src_alpha: i32,
    blend_dst_alpha: i32,
    color_mask: [bool; 4],
    clear_color: [f32; 4],
    dither_enabled: bool,
}

impl GlStateSnapshot {
    pub(crate) fn capture(gl: Arc<glow::Context>) -> Self {
        let caps = GlCapabilities::query(&gl);
        Self::capture_with_caps(gl, caps)
    }

    pub(crate) fn capture_with_caps(gl: Arc<glow::Context>, caps: GlCapabilities) -> Self {
        // SAFETY: callers create snapshots only while the host context is
        // current on its owning thread.
        unsafe {
            let unit_count = usize::from(caps.unit_count).clamp(1, 8);
            let active_texture = gl.get_parameter_i32(glow::ACTIVE_TEXTURE) as u32;
            let active_texture_binding = gl.get_parameter_i32(glow::TEXTURE_BINDING_2D);
            let active_sampler_binding = caps
                .supports_samplers
                .then(|| gl.get_parameter_i32(glow::SAMPLER_BINDING));
            let mut texture_units = [TextureUnitState::default(); 8];
            for (unit, slot) in texture_units.iter_mut().take(unit_count).enumerate() {
                gl.active_texture(glow::TEXTURE0 + unit as u32);
                *slot = TextureUnitState {
                    texture_2d: gl.get_parameter_i32(glow::TEXTURE_BINDING_2D),
                    sampler: caps
                        .supports_samplers
                        .then(|| gl.get_parameter_i32(glow::SAMPLER_BINDING)),
                };
            }
            gl.active_texture(active_texture);

            let unpack_alignment = gl.get_parameter_i32(glow::UNPACK_ALIGNMENT);
            let pack_alignment = gl.get_parameter_i32(glow::PACK_ALIGNMENT);
            let unpack_extras = caps.supports_unpack_extras.then(|| UnpackExtras {
                row_length: gl.get_parameter_i32(glow::UNPACK_ROW_LENGTH),
                skip_rows: gl.get_parameter_i32(glow::UNPACK_SKIP_ROWS),
                skip_pixels: gl.get_parameter_i32(glow::UNPACK_SKIP_PIXELS),
                pixel_unpack_buffer: gl.get_parameter_i32(glow::PIXEL_UNPACK_BUFFER_BINDING),
            });

            let mut viewport = [0; 4];
            gl.get_parameter_i32_slice(glow::VIEWPORT, &mut viewport);
            let mut scissor_box = [0; 4];
            gl.get_parameter_i32_slice(glow::SCISSOR_BOX, &mut scissor_box);
            let mut clear_color = [0.0; 4];
            gl.get_parameter_f32_slice(glow::COLOR_CLEAR_VALUE, &mut clear_color);

            Self {
                framebuffer: gl.get_parameter_i32(glow::FRAMEBUFFER_BINDING),
                read_framebuffer: caps
                    .supports_gl3
                    .then(|| gl.get_parameter_i32(glow::READ_FRAMEBUFFER_BINDING)),
                draw_framebuffer: caps
                    .supports_gl3
                    .then(|| gl.get_parameter_i32(glow::DRAW_FRAMEBUFFER_BINDING)),
                viewport,
                scissor_box,
                current_program: gl.get_parameter_i32(glow::CURRENT_PROGRAM),
                array_buffer: gl.get_parameter_i32(glow::ARRAY_BUFFER_BINDING),
                element_array_buffer: gl.get_parameter_i32(glow::ELEMENT_ARRAY_BUFFER_BINDING),
                vertex_array: caps
                    .supports_gl3
                    .then(|| gl.get_parameter_i32(glow::VERTEX_ARRAY_BINDING)),
                active_texture,
                texture_units,
                unit_count: unit_count as u8,
                active_texture_binding,
                active_sampler_binding,
                unpack_alignment,
                pack_alignment,
                unpack_extras,
                blend_enabled: gl.is_enabled(glow::BLEND),
                cull_enabled: gl.is_enabled(glow::CULL_FACE),
                depth_enabled: gl.is_enabled(glow::DEPTH_TEST),
                scissor_enabled: gl.is_enabled(glow::SCISSOR_TEST),
                stencil_enabled: gl.is_enabled(glow::STENCIL_TEST),
                framebuffer_srgb: (!caps.is_gles && caps.supports_gl3)
                    .then(|| gl.is_enabled(glow::FRAMEBUFFER_SRGB)),
                blend_equation_rgb: gl.get_parameter_i32(glow::BLEND_EQUATION_RGB),
                blend_equation_alpha: gl.get_parameter_i32(glow::BLEND_EQUATION_ALPHA),
                blend_src_rgb: gl.get_parameter_i32(glow::BLEND_SRC_RGB),
                blend_dst_rgb: gl.get_parameter_i32(glow::BLEND_DST_RGB),
                blend_src_alpha: gl.get_parameter_i32(glow::BLEND_SRC_ALPHA),
                blend_dst_alpha: gl.get_parameter_i32(glow::BLEND_DST_ALPHA),
                color_mask: gl.get_parameter_bool_array::<4>(glow::COLOR_WRITEMASK),
                clear_color,
                dither_enabled: gl.is_enabled(glow::DITHER),
                gl,
            }
        }
    }

    /// Reset OpenGL state to clean defaults before libmpv renders (for example, egui_glow leaves
    /// `GL_UNPACK_ALIGNMENT = 1`, which corrupts planar NV12/P010 texture uploads).
    pub(crate) fn reset_for_mpv(&self) {
        // SAFETY: callers invoke this only while the captured context is current on its owning thread.
        unsafe {
            self.gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
            self.gl.pixel_store_i32(glow::PACK_ALIGNMENT, 4);
            if self.unpack_extras.is_some() {
                self.gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
                self.gl.pixel_store_i32(glow::UNPACK_SKIP_ROWS, 0);
                self.gl.pixel_store_i32(glow::UNPACK_SKIP_PIXELS, 0);
                self.gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, None);
            }
            self.gl.disable(glow::SCISSOR_TEST);
            self.gl.disable(glow::BLEND);
            self.gl.disable(glow::DEPTH_TEST);
            self.gl.disable(glow::STENCIL_TEST);
            self.gl.disable(glow::CULL_FACE);
            self.gl.color_mask(true, true, true, true);
        }
    }

    pub(crate) fn restore_now(&self) {
        // SAFETY: this snapshot is only used while its owning context remains
        // current. The saved names were queried from that same context.
        unsafe {
            if let Some(read_framebuffer) = self.read_framebuffer {
                self.gl.bind_framebuffer(
                    glow::READ_FRAMEBUFFER,
                    framebuffer_from_i32(read_framebuffer),
                );
                self.gl.bind_framebuffer(
                    glow::DRAW_FRAMEBUFFER,
                    framebuffer_from_i32(self.draw_framebuffer.unwrap_or(self.framebuffer)),
                );
            } else {
                self.gl
                    .bind_framebuffer(glow::FRAMEBUFFER, framebuffer_from_i32(self.framebuffer));
            }
            self.gl.viewport(
                self.viewport[0],
                self.viewport[1],
                self.viewport[2],
                self.viewport[3],
            );
            self.gl.scissor(
                self.scissor_box[0],
                self.scissor_box[1],
                self.scissor_box[2],
                self.scissor_box[3],
            );
            self.gl.use_program(program_from_i32(self.current_program));
            if let Some(vertex_array) = self.vertex_array {
                self.gl
                    .bind_vertex_array(vertex_array_from_i32(vertex_array));
            }
            self.gl
                .bind_buffer(glow::ARRAY_BUFFER, buffer_from_i32(self.array_buffer));
            self.gl.bind_buffer(
                glow::ELEMENT_ARRAY_BUFFER,
                buffer_from_i32(self.element_array_buffer),
            );

            self.gl
                .pixel_store_i32(glow::UNPACK_ALIGNMENT, self.unpack_alignment);
            self.gl
                .pixel_store_i32(glow::PACK_ALIGNMENT, self.pack_alignment);
            if let Some(unpack) = self.unpack_extras {
                self.gl
                    .pixel_store_i32(glow::UNPACK_ROW_LENGTH, unpack.row_length);
                self.gl
                    .pixel_store_i32(glow::UNPACK_SKIP_ROWS, unpack.skip_rows);
                self.gl
                    .pixel_store_i32(glow::UNPACK_SKIP_PIXELS, unpack.skip_pixels);
                self.gl.bind_buffer(
                    glow::PIXEL_UNPACK_BUFFER,
                    buffer_from_i32(unpack.pixel_unpack_buffer),
                );
            }

            let unit_count = usize::from(self.unit_count).min(self.texture_units.len());
            for (unit, state) in self.texture_units[..unit_count].iter().enumerate() {
                self.gl.active_texture(glow::TEXTURE0 + unit as u32);
                self.gl
                    .bind_texture(glow::TEXTURE_2D, texture_from_i32(state.texture_2d));
                if let Some(sampler) = state.sampler {
                    self.gl.bind_sampler(unit as u32, sampler_from_i32(sampler));
                }
            }
            let active_index = self.active_texture.saturating_sub(glow::TEXTURE0) as usize;
            self.gl.active_texture(self.active_texture);
            if active_index >= unit_count {
                self.gl.bind_texture(
                    glow::TEXTURE_2D,
                    texture_from_i32(self.active_texture_binding),
                );
                if let Some(sampler) = self.active_sampler_binding {
                    self.gl
                        .bind_sampler(active_index as u32, sampler_from_i32(sampler));
                }
            }

            set_enabled(&self.gl, glow::BLEND, self.blend_enabled);
            set_enabled(&self.gl, glow::CULL_FACE, self.cull_enabled);
            set_enabled(&self.gl, glow::DEPTH_TEST, self.depth_enabled);
            set_enabled(&self.gl, glow::SCISSOR_TEST, self.scissor_enabled);
            set_enabled(&self.gl, glow::STENCIL_TEST, self.stencil_enabled);
            if let Some(enabled) = self.framebuffer_srgb {
                set_enabled(&self.gl, glow::FRAMEBUFFER_SRGB, enabled);
            }
            self.gl.blend_equation_separate(
                self.blend_equation_rgb as u32,
                self.blend_equation_alpha as u32,
            );
            self.gl.blend_func_separate(
                self.blend_src_rgb as u32,
                self.blend_dst_rgb as u32,
                self.blend_src_alpha as u32,
                self.blend_dst_alpha as u32,
            );
            self.gl.color_mask(
                self.color_mask[0],
                self.color_mask[1],
                self.color_mask[2],
                self.color_mask[3],
            );
            self.gl.clear_color(
                self.clear_color[0],
                self.clear_color[1],
                self.clear_color[2],
                self.clear_color[3],
            );
            set_enabled(&self.gl, glow::DITHER, self.dither_enabled);
        }
    }
}

impl Drop for GlStateSnapshot {
    fn drop(&mut self) {
        self.restore_now();
    }
}

impl GlStateRestore for GlStateSnapshot {}

fn parse_leading_u32(part: &str) -> Option<u32> {
    let mut value = 0_u32;
    let mut has_digit = false;
    for byte in part.bytes() {
        if !byte.is_ascii_digit() {
            break;
        }
        has_digit = true;
        value = value.checked_mul(10)?.checked_add(u32::from(byte - b'0'))?;
    }
    has_digit.then_some(value)
}

pub(crate) fn version_at_least(version: &str, required_major: u32, required_minor: u32) -> bool {
    version.split_whitespace().any(|token| {
        let mut parts = token.split('.');
        let Some(major) = parts.next().and_then(|part| part.parse::<u32>().ok()) else {
            return false;
        };
        let Some(minor) = parts.next().and_then(parse_leading_u32) else {
            return false;
        };
        (major, minor) >= (required_major, required_minor)
    })
}

unsafe fn set_enabled(gl: &glow::Context, capability: u32, enabled: bool) {
    if enabled {
        unsafe { gl.enable(capability) };
    } else {
        unsafe { gl.disable(capability) };
    }
}

fn framebuffer_from_i32(raw: i32) -> Option<glow::Framebuffer> {
    nonzero_name(raw).map(glow::NativeFramebuffer)
}

fn texture_from_i32(raw: i32) -> Option<glow::Texture> {
    nonzero_name(raw).map(glow::NativeTexture)
}

fn program_from_i32(raw: i32) -> Option<glow::Program> {
    nonzero_name(raw).map(glow::NativeProgram)
}

fn buffer_from_i32(raw: i32) -> Option<glow::Buffer> {
    nonzero_name(raw).map(glow::NativeBuffer)
}

fn vertex_array_from_i32(raw: i32) -> Option<glow::VertexArray> {
    nonzero_name(raw).map(glow::NativeVertexArray)
}

fn sampler_from_i32(raw: i32) -> Option<glow::Sampler> {
    nonzero_name(raw).map(glow::NativeSampler)
}

fn nonzero_name(raw: i32) -> Option<NonZeroU32> {
    u32::try_from(raw).ok().and_then(NonZeroU32::new)
}

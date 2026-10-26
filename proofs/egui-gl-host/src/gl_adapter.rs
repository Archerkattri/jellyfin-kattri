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

pub(crate) struct HostGlContextProvider {
    parts: Rc<ContextParts>,
    gl: Arc<glow::Context>,
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
        Arc::new(Self {
            parts,
            gl,
            owner: thread::current().id(),
        })
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
        Box::new(GlStateSnapshot::capture(Arc::clone(&self.provider.gl)))
    }
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
    texture_zero: i32,
    active_texture_binding: i32,
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
        // SAFETY: callers create snapshots only while the host context is
        // current on its owning thread.
        unsafe {
            let version = gl.get_parameter_string(glow::VERSION);
            let is_gles = version.contains("OpenGL ES");
            let supports_gl3 = version_at_least(&version, 3, 0);
            let active_texture = gl.get_parameter_i32(glow::ACTIVE_TEXTURE) as u32;
            let active_texture_binding = gl.get_parameter_i32(glow::TEXTURE_BINDING_2D);
            gl.active_texture(glow::TEXTURE0);
            let texture_zero = gl.get_parameter_i32(glow::TEXTURE_BINDING_2D);
            gl.active_texture(active_texture);

            let mut viewport = [0; 4];
            gl.get_parameter_i32_slice(glow::VIEWPORT, &mut viewport);
            let mut scissor_box = [0; 4];
            gl.get_parameter_i32_slice(glow::SCISSOR_BOX, &mut scissor_box);
            let mut clear_color = [0.0; 4];
            gl.get_parameter_f32_slice(glow::COLOR_CLEAR_VALUE, &mut clear_color);

            Self {
                framebuffer: gl.get_parameter_i32(glow::FRAMEBUFFER_BINDING),
                read_framebuffer: supports_gl3
                    .then(|| gl.get_parameter_i32(glow::READ_FRAMEBUFFER_BINDING)),
                draw_framebuffer: supports_gl3
                    .then(|| gl.get_parameter_i32(glow::DRAW_FRAMEBUFFER_BINDING)),
                viewport,
                scissor_box,
                current_program: gl.get_parameter_i32(glow::CURRENT_PROGRAM),
                array_buffer: gl.get_parameter_i32(glow::ARRAY_BUFFER_BINDING),
                element_array_buffer: gl.get_parameter_i32(glow::ELEMENT_ARRAY_BUFFER_BINDING),
                vertex_array: supports_gl3
                    .then(|| gl.get_parameter_i32(glow::VERTEX_ARRAY_BINDING)),
                active_texture,
                texture_zero,
                active_texture_binding,
                blend_enabled: gl.is_enabled(glow::BLEND),
                cull_enabled: gl.is_enabled(glow::CULL_FACE),
                depth_enabled: gl.is_enabled(glow::DEPTH_TEST),
                scissor_enabled: gl.is_enabled(glow::SCISSOR_TEST),
                stencil_enabled: gl.is_enabled(glow::STENCIL_TEST),
                framebuffer_srgb: (!is_gles && supports_gl3)
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

            self.gl.active_texture(glow::TEXTURE0);
            self.gl
                .bind_texture(glow::TEXTURE_2D, texture_from_i32(self.texture_zero));
            if self.active_texture != glow::TEXTURE0 {
                self.gl.active_texture(self.active_texture);
                self.gl.bind_texture(
                    glow::TEXTURE_2D,
                    texture_from_i32(self.active_texture_binding),
                );
            }
            self.gl.active_texture(self.active_texture);

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

fn version_at_least(version: &str, required_major: u32, required_minor: u32) -> bool {
    version.split_whitespace().any(|token| {
        let mut parts = token.split('.');
        let Some(major) = parts.next().and_then(|part| part.parse::<u32>().ok()) else {
            return false;
        };
        let Some(minor) = parts.next().and_then(|part| {
            part.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse::<u32>()
                .ok()
        }) else {
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

fn nonzero_name(raw: i32) -> Option<NonZeroU32> {
    u32::try_from(raw).ok().and_then(NonZeroU32::new)
}

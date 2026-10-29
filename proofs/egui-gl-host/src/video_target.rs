use std::sync::Arc;

use glow::HasContext;

use crate::gl_adapter::GlStateSnapshot;

pub(crate) struct VideoTarget {
    framebuffer: glow::Framebuffer,
    texture: glow::Texture,
    size: [u32; 2],
    internal_format: u32,
}

impl VideoTarget {
    pub(crate) fn new(gl: &Arc<glow::Context>, size: [u32; 2]) -> Result<Self, String> {
        if size[0] == 0 || size[1] == 0 || size[0] > i32::MAX as u32 || size[1] > i32::MAX as u32 {
            return Err("video FBO dimensions must be non-zero and fit OpenGL".into());
        }
        // SAFETY: this is called on the host thread while its glutin context is
        // current. The snapshot restores the state used by egui_glow.
        let _state = GlStateSnapshot::capture(Arc::clone(gl));
        let max_texture_size =
            unsafe { gl.get_parameter_i32(glow::MAX_TEXTURE_SIZE).max(0) } as u32;
        if size[0] > max_texture_size || size[1] > max_texture_size {
            return Err(format!(
                "video FBO {}x{} exceeds GL_MAX_TEXTURE_SIZE {max_texture_size}",
                size[0], size[1]
            ));
        }

        let version = unsafe { gl.get_parameter_string(glow::VERSION) };
        let internal_format = if version.contains("OpenGL ES 2.") {
            glow::RGBA
        } else {
            glow::RGBA8
        };

        // SAFETY: the current context owns every object created below.
        let texture = unsafe { gl.create_texture()? };
        let framebuffer = match unsafe { gl.create_framebuffer() } {
            Ok(framebuffer) => framebuffer,
            Err(error) => {
                unsafe { gl.delete_texture(texture) };
                return Err(error);
            }
        };

        unsafe {
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MIN_FILTER,
                glow::LINEAR as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MAG_FILTER,
                glow::LINEAR as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_S,
                glow::CLAMP_TO_EDGE as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_T,
                glow::CLAMP_TO_EDGE as i32,
            );
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                internal_format as i32,
                size[0] as i32,
                size[1] as i32,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(None),
            );
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
            gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(texture),
                0,
            );
        }
        let status = unsafe { gl.check_framebuffer_status(glow::FRAMEBUFFER) };
        if status != glow::FRAMEBUFFER_COMPLETE {
            unsafe {
                gl.delete_framebuffer(framebuffer);
                gl.delete_texture(texture);
            }
            return Err(format!("video FBO is incomplete (status 0x{status:04x})"));
        }

        let target = Self {
            framebuffer,
            texture,
            size,
            internal_format,
        };
        target.clear_black(gl);
        Ok(target)
    }

    /// Reallocate this target's existing texture and FBO for a new size. Reusing the same GL
    /// names avoids egui_glow's deferred texture destruction queuing one full-size texture per
    /// resize frame.
    pub(crate) fn resize(&mut self, gl: &Arc<glow::Context>, size: [u32; 2]) -> Result<(), String> {
        if size == self.size {
            return Ok(());
        }
        if size[0] == 0 || size[1] == 0 || size[0] > i32::MAX as u32 || size[1] > i32::MAX as u32 {
            return Err("video FBO dimensions must be non-zero and fit OpenGL".into());
        }
        // SAFETY: called on the host thread with its glutin context current; the snapshot
        // restores the state egui_glow expects.
        let _state = GlStateSnapshot::capture(Arc::clone(gl));
        let max_texture_size =
            unsafe { gl.get_parameter_i32(glow::MAX_TEXTURE_SIZE).max(0) } as u32;
        if size[0] > max_texture_size || size[1] > max_texture_size {
            return Err(format!(
                "video FBO {}x{} exceeds GL_MAX_TEXTURE_SIZE {max_texture_size}",
                size[0], size[1]
            ));
        }
        unsafe {
            gl.bind_texture(glow::TEXTURE_2D, Some(self.texture));
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                self.internal_format as i32,
                size[0] as i32,
                size[1] as i32,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(None),
            );
        }
        self.size = size;
        self.clear_black(gl);
        Ok(())
    }

    pub(crate) fn texture(&self) -> glow::Texture {
        self.texture
    }

    pub(crate) fn size(&self) -> [u32; 2] {
        self.size
    }

    pub(crate) fn framebuffer_id(&self) -> u32 {
        self.framebuffer.0.get()
    }

    pub(crate) fn internal_format(&self) -> u32 {
        self.internal_format
    }

    pub(crate) fn clear_black(&self, gl: &glow::Context) {
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.framebuffer));
            gl.disable(glow::SCISSOR_TEST);
            gl.color_mask(true, true, true, true);
            gl.clear_color(0.0, 0.0, 0.0, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
        }
    }

    /// The native texture is owned by egui_glow after it is registered there;
    /// this target owns only the FBO attachment relationship.
    pub(crate) fn destroy_framebuffer(self, gl: &glow::Context) {
        unsafe { gl.delete_framebuffer(self.framebuffer) };
    }
}

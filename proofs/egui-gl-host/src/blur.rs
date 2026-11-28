use std::sync::Arc;

use egui::Color32;
use glow::HasContext;

use crate::gl_adapter::{GlStateSnapshot, version_at_least};

/// Reference to the downsampled blurred video frame for frosted-glass overlays.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BackdropBlur {
    pub(crate) texture_id: egui::TextureId,
    pub(crate) stage_rect: egui::Rect,
}

const FROSTED_CORNER_RADIUS: u8 = 12;

fn frosted_glass_fill() -> Color32 {
    Color32::from_rgba_unmultiplied(16, 18, 22, 175)
}

fn frosted_glass_fallback_fill() -> Color32 {
    Color32::from_rgba_unmultiplied(16, 18, 22, 220)
}

fn frosted_glass_border() -> Color32 {
    Color32::from_rgba_unmultiplied(255, 255, 255, 34)
}

/// Paint `add_contents` inside a rounded frosted-glass frame with optional blurred video backdrop.
pub(crate) fn show_frosted_glass_frame<R>(
    ui: &mut egui::Ui,
    backdrop: Option<BackdropBlur>,
    opacity: f32,
    margin: egui::Margin,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<R> {
    let opacity = opacity.clamp(0.0, 1.0);
    let fill = if backdrop.is_some() {
        frosted_glass_fill()
    } else {
        frosted_glass_fallback_fill()
    }
    .gamma_multiply(opacity);
    let border = frosted_glass_border().gamma_multiply(opacity);
    let rounding = egui::CornerRadius::same(FROSTED_CORNER_RADIUS);

    let bg_idx = ui.painter().add(egui::Shape::Noop);
    let response = egui::Frame::new()
        .fill(fill)
        .stroke(egui::Stroke::new(1.0, border))
        .corner_radius(rounding)
        .inner_margin(margin)
        .show(ui, add_contents);

    if let Some(blur) = backdrop
        && let Some(shape) = backdrop_blur_shape(blur, response.response.rect, rounding, opacity)
    {
        ui.painter().set(bg_idx, shape);
    }

    response
}

fn backdrop_blur_shape(
    blur: BackdropBlur,
    panel_rect: egui::Rect,
    rounding: egui::CornerRadius,
    opacity: f32,
) -> Option<egui::Shape> {
    if blur.stage_rect.width() <= 0.0 || blur.stage_rect.height() <= 0.0 {
        return None;
    }
    let clipped = panel_rect.intersect(blur.stage_rect);
    if !clipped.is_positive() {
        return None;
    }
    let uv = egui::Rect::from_min_max(
        egui::pos2(
            ((clipped.left() - blur.stage_rect.left()) / blur.stage_rect.width()).clamp(0.0, 1.0),
            ((clipped.top() - blur.stage_rect.top()) / blur.stage_rect.height()).clamp(0.0, 1.0),
        ),
        egui::pos2(
            ((clipped.right() - blur.stage_rect.left()) / blur.stage_rect.width()).clamp(0.0, 1.0),
            ((clipped.bottom() - blur.stage_rect.top()) / blur.stage_rect.height()).clamp(0.0, 1.0),
        ),
    );
    let tint = Color32::WHITE.gamma_multiply(opacity.clamp(0.0, 1.0));
    Some(egui::Shape::Rect(
        egui::epaint::RectShape::filled(clipped, rounding, tint).with_texture(blur.texture_id, uv),
    ))
}

/// Lightweight 2-pass separable Gaussian blur operating on `VideoTarget::texture()` into a
/// 1/4-resolution RGBA8 FBO texture.
pub(crate) struct VideoBlurTarget {
    gl: Arc<glow::Context>,
    program: glow::Program,
    u_tex: glow::UniformLocation,
    u_texel_step: glow::UniformLocation,
    vbo: glow::Buffer,
    vao: Option<glow::VertexArray>,
    scratch_fbo: glow::Framebuffer,
    scratch_texture: glow::Texture,
    output_fbo: glow::Framebuffer,
    output_texture: glow::Texture,
    owns_output_texture: bool,
    size: [u32; 2],
    internal_format: u32,
    supports_samplers: bool,
    supports_pbo: bool,
}

impl VideoBlurTarget {
    pub(crate) fn new(gl: Arc<glow::Context>, source_size: [u32; 2]) -> Result<Self, String> {
        let blur_size = quarter_size(source_size);
        let _state = GlStateSnapshot::capture(Arc::clone(&gl));

        let max_texture_size =
            unsafe { gl.get_parameter_i32(glow::MAX_TEXTURE_SIZE).max(0) } as u32;
        if blur_size[0] > max_texture_size || blur_size[1] > max_texture_size {
            return Err(format!(
                "blur FBO {}x{} exceeds GL_MAX_TEXTURE_SIZE {max_texture_size}",
                blur_size[0], blur_size[1]
            ));
        }

        let version = unsafe { gl.get_parameter_string(glow::VERSION) };
        let is_gles = version.contains("OpenGL ES");
        let supports_gl3 = version_at_least(&version, 3, 0);
        let supports_vao = !is_gles || supports_gl3;
        let supports_samplers =
            (is_gles && supports_gl3) || (!is_gles && version_at_least(&version, 3, 3));
        let supports_pbo = !is_gles || supports_gl3;
        let internal_format = if version.contains("OpenGL ES 2.") {
            glow::RGBA
        } else {
            glow::RGBA8
        };

        let (vert_src, frag_src) = shader_sources(&version, is_gles, supports_gl3);
        let program = compile_blur_program(&gl, vert_src, frag_src)?;

        let Some(u_tex) = (unsafe { gl.get_uniform_location(program, "u_tex") }) else {
            unsafe { gl.delete_program(program) };
            return Err("blur shader missing u_tex uniform".into());
        };
        let Some(u_texel_step) = (unsafe { gl.get_uniform_location(program, "u_texel_step") })
        else {
            unsafe { gl.delete_program(program) };
            return Err("blur shader missing u_texel_step uniform".into());
        };

        let vbo = match unsafe { gl.create_buffer() } {
            Ok(vbo) => vbo,
            Err(err) => {
                unsafe { gl.delete_program(program) };
                return Err(err);
            }
        };
        let vao = if supports_vao {
            match unsafe { gl.create_vertex_array() } {
                Ok(vao) => Some(vao),
                Err(err) => {
                    unsafe {
                        gl.delete_buffer(vbo);
                        gl.delete_program(program);
                    }
                    return Err(err);
                }
            }
        } else {
            None
        };

        let quad: [f32; 8] = [-1.0, -1.0, 1.0, -1.0, -1.0, 1.0, 1.0, 1.0];
        let quad_bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(quad.as_ptr().cast::<u8>(), std::mem::size_of_val(&quad))
        };
        unsafe {
            if let Some(vao) = vao {
                gl.bind_vertex_array(Some(vao));
            }
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, quad_bytes, glow::STATIC_DRAW);
            gl.enable_vertex_attrib_array(0);
            gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 8, 0);
        }

        let (scratch_fbo, scratch_texture) =
            match create_color_target(&gl, blur_size, internal_format, "blur scratch") {
                Ok(pair) => pair,
                Err(err) => {
                    unsafe {
                        if let Some(vao) = vao {
                            gl.delete_vertex_array(vao);
                        }
                        gl.delete_buffer(vbo);
                        gl.delete_program(program);
                    }
                    return Err(err);
                }
            };

        let (output_fbo, output_texture) =
            match create_color_target(&gl, blur_size, internal_format, "blur output") {
                Ok(pair) => pair,
                Err(err) => {
                    unsafe {
                        gl.delete_framebuffer(scratch_fbo);
                        gl.delete_texture(scratch_texture);
                        if let Some(vao) = vao {
                            gl.delete_vertex_array(vao);
                        }
                        gl.delete_buffer(vbo);
                        gl.delete_program(program);
                    }
                    return Err(err);
                }
            };

        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(output_fbo));
            gl.disable(glow::SCISSOR_TEST);
            gl.color_mask(true, true, true, true);
            gl.clear_color(0.0, 0.0, 0.0, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
        }

        Ok(Self {
            gl,
            program,
            u_tex,
            u_texel_step,
            vbo,
            vao,
            scratch_fbo,
            scratch_texture,
            output_fbo,
            output_texture,
            owns_output_texture: true,
            size: blur_size,
            internal_format,
            supports_samplers,
            supports_pbo,
        })
    }

    pub(crate) fn output_texture(&self) -> glow::Texture {
        self.output_texture
    }

    /// Marks `output_texture` as registered with `egui_glow::Painter`, which frees registered
    /// native textures when destroyed.
    pub(crate) fn detach_output_texture(&mut self) {
        self.owns_output_texture = false;
    }

    /// Run the 2-pass separable Gaussian blur from `source` (`source_size`) into `output_texture`.
    pub(crate) fn update(
        &mut self,
        source: glow::Texture,
        source_size: [u32; 2],
    ) -> Result<(), String> {
        let blur_size = quarter_size(source_size);
        let _state = GlStateSnapshot::capture(Arc::clone(&self.gl));

        if blur_size != self.size {
            self.resize_textures(blur_size)?;
        }

        let [w, h] = self.size;
        unsafe {
            if self.supports_pbo {
                self.gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, None);
            }
            self.gl.disable(glow::SCISSOR_TEST);
            self.gl.disable(glow::BLEND);
            self.gl.disable(glow::DEPTH_TEST);
            self.gl.disable(glow::STENCIL_TEST);
            self.gl.disable(glow::CULL_FACE);
            self.gl.color_mask(true, true, true, true);
            self.gl.viewport(0, 0, w as i32, h as i32);

            self.gl.use_program(Some(self.program));
            if let Some(vao) = self.vao {
                self.gl.bind_vertex_array(Some(vao));
            } else {
                self.gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.vbo));
                self.gl.enable_vertex_attrib_array(0);
                self.gl
                    .vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 8, 0);
            }
            self.gl.active_texture(glow::TEXTURE0);
            if self.supports_samplers {
                self.gl.bind_sampler(0, None);
            }
            self.gl.uniform_1_i32(Some(&self.u_tex), 0);

            // Pass 1: horizontal blur from source video texture -> scratch FBO.
            self.gl
                .bind_framebuffer(glow::FRAMEBUFFER, Some(self.scratch_fbo));
            self.gl.bind_texture(glow::TEXTURE_2D, Some(source));
            self.gl
                .uniform_2_f32(Some(&self.u_texel_step), 1.5 / w as f32, 0.0);
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

            // Pass 2: vertical blur from scratch texture -> output FBO.
            self.gl
                .bind_framebuffer(glow::FRAMEBUFFER, Some(self.output_fbo));
            self.gl
                .bind_texture(glow::TEXTURE_2D, Some(self.scratch_texture));
            self.gl
                .uniform_2_f32(Some(&self.u_texel_step), 0.0, 1.5 / h as f32);
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        }
        Ok(())
    }

    fn resize_textures(&mut self, blur_size: [u32; 2]) -> Result<(), String> {
        let max_texture_size =
            unsafe { self.gl.get_parameter_i32(glow::MAX_TEXTURE_SIZE).max(0) } as u32;
        if blur_size[0] > max_texture_size || blur_size[1] > max_texture_size {
            return Err(format!(
                "blur FBO {}x{} exceeds GL_MAX_TEXTURE_SIZE {max_texture_size}",
                blur_size[0], blur_size[1]
            ));
        }
        for texture in [self.scratch_texture, self.output_texture] {
            unsafe {
                self.gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                self.gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    self.internal_format as i32,
                    blur_size[0] as i32,
                    blur_size[1] as i32,
                    0,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(None),
                );
            }
        }
        self.size = blur_size;
        Ok(())
    }
}

impl Drop for VideoBlurTarget {
    fn drop(&mut self) {
        unsafe {
            self.gl.delete_framebuffer(self.scratch_fbo);
            self.gl.delete_framebuffer(self.output_fbo);
            self.gl.delete_texture(self.scratch_texture);
            if self.owns_output_texture {
                self.gl.delete_texture(self.output_texture);
            }
            if let Some(vao) = self.vao {
                self.gl.delete_vertex_array(vao);
            }
            self.gl.delete_buffer(self.vbo);
            self.gl.delete_program(self.program);
        }
    }
}

fn quarter_size(source: [u32; 2]) -> [u32; 2] {
    [(source[0] / 4).max(1), (source[1] / 4).max(1)]
}

fn create_color_target(
    gl: &glow::Context,
    size: [u32; 2],
    internal_format: u32,
    label: &str,
) -> Result<(glow::Framebuffer, glow::Texture), String> {
    let texture = unsafe { gl.create_texture()? };
    let framebuffer = match unsafe { gl.create_framebuffer() } {
        Ok(fbo) => fbo,
        Err(err) => {
            unsafe { gl.delete_texture(texture) };
            return Err(err);
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
        return Err(format!("{label} FBO is incomplete (status 0x{status:04x})"));
    }
    Ok((framebuffer, texture))
}

fn shader_sources(
    version: &str,
    is_gles: bool,
    supports_gl3: bool,
) -> (&'static str, &'static str) {
    if is_gles {
        if supports_gl3 {
            (
                "#version 300 es
layout(location = 0) in vec2 a_pos;
out vec2 v_uv;
void main() {
    v_uv = a_pos * 0.5 + 0.5;
    gl_Position = vec4(a_pos, 0.0, 1.0);
}",
                "#version 300 es
precision mediump float;
in vec2 v_uv;
out vec4 frag_color;
uniform sampler2D u_tex;
uniform vec2 u_texel_step;
void main() {
    vec3 sum = texture(u_tex, v_uv).rgb * 0.2270270270;
    sum += texture(u_tex, v_uv + u_texel_step * 1.3846153846).rgb * 0.3162162162;
    sum += texture(u_tex, v_uv - u_texel_step * 1.3846153846).rgb * 0.3162162162;
    sum += texture(u_tex, v_uv + u_texel_step * 3.2307692308).rgb * 0.0702702703;
    sum += texture(u_tex, v_uv - u_texel_step * 3.2307692308).rgb * 0.0702702703;
    frag_color = vec4(sum, 1.0);
}",
            )
        } else {
            (
                "#version 100
attribute vec2 a_pos;
varying vec2 v_uv;
void main() {
    v_uv = a_pos * 0.5 + 0.5;
    gl_Position = vec4(a_pos, 0.0, 1.0);
}",
                "#version 100
precision mediump float;
varying vec2 v_uv;
uniform sampler2D u_tex;
uniform vec2 u_texel_step;
void main() {
    vec3 sum = texture2D(u_tex, v_uv).rgb * 0.2270270270;
    sum += texture2D(u_tex, v_uv + u_texel_step * 1.3846153846).rgb * 0.3162162162;
    sum += texture2D(u_tex, v_uv - u_texel_step * 1.3846153846).rgb * 0.3162162162;
    sum += texture2D(u_tex, v_uv + u_texel_step * 3.2307692308).rgb * 0.0702702703;
    sum += texture2D(u_tex, v_uv - u_texel_step * 3.2307692308).rgb * 0.0702702703;
    gl_FragColor = vec4(sum, 1.0);
}",
            )
        }
    } else if version_at_least(version, 3, 3) {
        (
            "#version 330 core
layout(location = 0) in vec2 a_pos;
out vec2 v_uv;
void main() {
    v_uv = a_pos * 0.5 + 0.5;
    gl_Position = vec4(a_pos, 0.0, 1.0);
}",
            "#version 330 core
in vec2 v_uv;
out vec4 frag_color;
uniform sampler2D u_tex;
uniform vec2 u_texel_step;
void main() {
    vec3 sum = texture(u_tex, v_uv).rgb * 0.2270270270;
    sum += texture(u_tex, v_uv + u_texel_step * 1.3846153846).rgb * 0.3162162162;
    sum += texture(u_tex, v_uv - u_texel_step * 1.3846153846).rgb * 0.3162162162;
    sum += texture(u_tex, v_uv + u_texel_step * 3.2307692308).rgb * 0.0702702703;
    sum += texture(u_tex, v_uv - u_texel_step * 3.2307692308).rgb * 0.0702702703;
    frag_color = vec4(sum, 1.0);
}",
        )
    } else {
        (
            "#version 120
attribute vec2 a_pos;
varying vec2 v_uv;
void main() {
    v_uv = a_pos * 0.5 + 0.5;
    gl_Position = vec4(a_pos, 0.0, 1.0);
}",
            "#version 120
varying vec2 v_uv;
uniform sampler2D u_tex;
uniform vec2 u_texel_step;
void main() {
    vec3 sum = texture2D(u_tex, v_uv).rgb * 0.2270270270;
    sum += texture2D(u_tex, v_uv + u_texel_step * 1.3846153846).rgb * 0.3162162162;
    sum += texture2D(u_tex, v_uv - u_texel_step * 1.3846153846).rgb * 0.3162162162;
    sum += texture2D(u_tex, v_uv + u_texel_step * 3.2307692308).rgb * 0.0702702703;
    sum += texture2D(u_tex, v_uv - u_texel_step * 3.2307692308).rgb * 0.0702702703;
    gl_FragColor = vec4(sum, 1.0);
}",
        )
    }
}

fn compile_blur_program(
    gl: &glow::Context,
    vertex_src: &str,
    fragment_src: &str,
) -> Result<glow::Program, String> {
    let program = unsafe { gl.create_program()? };
    let vert = match compile_shader(gl, glow::VERTEX_SHADER, vertex_src) {
        Ok(shader) => shader,
        Err(err) => {
            unsafe { gl.delete_program(program) };
            return Err(err);
        }
    };
    let frag = match compile_shader(gl, glow::FRAGMENT_SHADER, fragment_src) {
        Ok(shader) => shader,
        Err(err) => {
            unsafe {
                gl.delete_shader(vert);
                gl.delete_program(program);
            }
            return Err(err);
        }
    };
    unsafe {
        gl.attach_shader(program, vert);
        gl.attach_shader(program, frag);
        gl.bind_attrib_location(program, 0, "a_pos");
        gl.link_program(program);
        gl.detach_shader(program, vert);
        gl.detach_shader(program, frag);
        gl.delete_shader(vert);
        gl.delete_shader(frag);
    }
    if !unsafe { gl.get_program_link_status(program) } {
        let log = unsafe { gl.get_program_info_log(program) };
        unsafe { gl.delete_program(program) };
        return Err(format!("blur shader link failed: {log}"));
    }
    Ok(program)
}

fn compile_shader(
    gl: &glow::Context,
    shader_type: u32,
    source: &str,
) -> Result<glow::Shader, String> {
    let shader = unsafe { gl.create_shader(shader_type)? };
    unsafe {
        gl.shader_source(shader, source);
        gl.compile_shader(shader);
    }
    if !unsafe { gl.get_shader_compile_status(shader) } {
        let log = unsafe { gl.get_shader_info_log(shader) };
        unsafe { gl.delete_shader(shader) };
        return Err(format!("blur shader compile failed: {log}"));
    }
    Ok(shader)
}

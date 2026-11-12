//! YUV→RGB conversion in an OpenGL shader so the viewer does not spend tens
//! of milliseconds in CPU swscale on older machines.

use eframe::egui;
use eframe::glow::{self, HasContext};
use mirrorwire_media::decode::{YuvImage, YuvLayout};
use std::sync::{Arc, Mutex};

pub struct YuvPaint {
    image: Arc<YuvImage>,
    renderer: Arc<Mutex<Option<YuvRenderer>>>,
    nearest: bool,
}

impl YuvPaint {
    pub fn new(
        image: Arc<YuvImage>,
        renderer: Arc<Mutex<Option<YuvRenderer>>>,
        nearest: bool,
    ) -> Self {
        Self {
            image,
            renderer,
            nearest,
        }
    }

    pub fn callback(self, rect: egui::Rect) -> egui::PaintCallback {
        egui::PaintCallback {
            rect,
            callback: Arc::new(eframe::egui_glow::CallbackFn::new(move |_info, painter| {
                let gl = painter.gl();
                let mut slot = self.renderer.lock().expect("yuv renderer");
                if slot.is_none() {
                    match unsafe { YuvRenderer::new(gl) } {
                        Ok(r) => *slot = Some(r),
                        Err(e) => {
                            tracing::warn!("YUV GL shader unavailable ({e}); CPU convert fallback");
                            return;
                        }
                    }
                }
                if let Some(r) = slot.as_mut() {
                    unsafe { r.draw(gl, &self.image, self.nearest) };
                }
            })),
        }
    }
}

pub struct YuvRenderer {
    program: glow::Program,
    vao: glow::VertexArray,
    tex_y: glow::Texture,
    tex_u: glow::Texture,
    tex_v: glow::Texture,
    loc_y: Option<glow::UniformLocation>,
    loc_u: Option<glow::UniformLocation>,
    loc_v: Option<glow::UniformLocation>,
    loc_nv12: Option<glow::UniformLocation>,
}

impl YuvRenderer {
    unsafe fn new(gl: &glow::Context) -> Result<Self, String> {
        let vs = compile(gl, glow::VERTEX_SHADER, VERT)?;
        let fs = compile(gl, glow::FRAGMENT_SHADER, FRAG)?;
        let program = gl.create_program().map_err(|e| e.to_string())?;
        gl.attach_shader(program, vs);
        gl.attach_shader(program, fs);
        gl.link_program(program);
        if !gl.get_program_link_status(program) {
            let log = gl.get_program_info_log(program);
            gl.delete_program(program);
            return Err(log);
        }
        gl.delete_shader(vs);
        gl.delete_shader(fs);

        let vao = gl.create_vertex_array().map_err(|e| e.to_string())?;
        let tex_y = gl.create_texture().map_err(|e| e.to_string())?;
        let tex_u = gl.create_texture().map_err(|e| e.to_string())?;
        let tex_v = gl.create_texture().map_err(|e| e.to_string())?;
        Ok(Self {
            loc_y: gl.get_uniform_location(program, "u_y"),
            loc_u: gl.get_uniform_location(program, "u_u"),
            loc_v: gl.get_uniform_location(program, "u_v"),
            loc_nv12: gl.get_uniform_location(program, "u_nv12"),
            program,
            vao,
            tex_y,
            tex_u,
            tex_v,
        })
    }

    unsafe fn draw(&mut self, gl: &glow::Context, img: &YuvImage, nearest: bool) {
        let filter = if nearest { glow::NEAREST } else { glow::LINEAR };
        upload_r8(
            gl,
            self.tex_y,
            img.width,
            img.height,
            img.y_stride,
            &img.y,
            filter,
        );
        match img.layout {
            YuvLayout::Yuv420 => {
                let (cw, ch) = (img.width.div_ceil(2), img.height.div_ceil(2));
                upload_r8(gl, self.tex_u, cw, ch, img.u_stride, &img.u, filter);
                upload_r8(gl, self.tex_v, cw, ch, img.v_stride, &img.v, filter);
            }
            YuvLayout::Nv12 => {
                let (cw, ch) = (img.width.div_ceil(2), img.height.div_ceil(2));
                // U and V packed as RG in `u` (see YuvImage::from_frame).
                upload_rg8(gl, self.tex_u, cw, ch, img.u_stride, &img.u, filter);
            }
        }

        gl.use_program(Some(self.program));
        gl.uniform_1_i32(self.loc_y.as_ref(), 0);
        gl.uniform_1_i32(self.loc_u.as_ref(), 1);
        gl.uniform_1_i32(self.loc_v.as_ref(), 2);
        gl.uniform_1_i32(
            self.loc_nv12.as_ref(),
            i32::from(matches!(img.layout, YuvLayout::Nv12)),
        );
        gl.active_texture(glow::TEXTURE0);
        gl.bind_texture(glow::TEXTURE_2D, Some(self.tex_y));
        gl.active_texture(glow::TEXTURE1);
        gl.bind_texture(glow::TEXTURE_2D, Some(self.tex_u));
        gl.active_texture(glow::TEXTURE2);
        gl.bind_texture(glow::TEXTURE_2D, Some(self.tex_v));
        gl.bind_vertex_array(Some(self.vao));
        gl.disable(glow::BLEND);
        gl.draw_arrays(glow::TRIANGLES, 0, 3);
        gl.bind_vertex_array(None);
        gl.use_program(None);
        gl.active_texture(glow::TEXTURE0);
        gl.enable(glow::BLEND);
    }
}

unsafe fn upload_r8(
    gl: &glow::Context,
    tex: glow::Texture,
    w: u32,
    h: u32,
    stride: usize,
    data: &[u8],
    filter: u32,
) {
    gl.bind_texture(glow::TEXTURE_2D, Some(tex));
    gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
    gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, stride as i32);
    gl.tex_image_2d(
        glow::TEXTURE_2D,
        0,
        glow::R8 as i32,
        w as i32,
        h as i32,
        0,
        glow::RED,
        glow::UNSIGNED_BYTE,
        glow::PixelUnpackData::Slice(Some(data)),
    );
    gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
    gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, filter as i32);
    gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, filter as i32);
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
}

unsafe fn upload_rg8(
    gl: &glow::Context,
    tex: glow::Texture,
    w: u32,
    h: u32,
    stride: usize,
    data: &[u8],
    filter: u32,
) {
    gl.bind_texture(glow::TEXTURE_2D, Some(tex));
    gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
    gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, (stride / 2) as i32);
    gl.tex_image_2d(
        glow::TEXTURE_2D,
        0,
        glow::RG8 as i32,
        w as i32,
        h as i32,
        0,
        glow::RG,
        glow::UNSIGNED_BYTE,
        glow::PixelUnpackData::Slice(Some(data)),
    );
    gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
    gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, filter as i32);
    gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, filter as i32);
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
}

unsafe fn compile(gl: &glow::Context, kind: u32, src: &str) -> Result<glow::Shader, String> {
    let sh = gl.create_shader(kind).map_err(|e| e.to_string())?;
    gl.shader_source(sh, src);
    gl.compile_shader(sh);
    if !gl.get_shader_compile_status(sh) {
        let log = gl.get_shader_info_log(sh);
        gl.delete_shader(sh);
        return Err(log);
    }
    Ok(sh)
}

// GLSL 150 runs on OpenGL 3.2 (2011 iMac / HD 6770M).
const VERT: &str = r#"#version 150
const vec2 POS[3] = vec2[](vec2(-1.0, -1.0), vec2(3.0, -1.0), vec2(-1.0, 3.0));
const vec2 UV[3]  = vec2[](vec2( 0.0,  1.0), vec2(2.0,  1.0), vec2( 0.0,-1.0));
out vec2 v_uv;
void main() {
    v_uv = UV[gl_VertexID];
    gl_Position = vec4(POS[gl_VertexID], 0.0, 1.0);
}
"#;

// BT.709 full-range (desktop capture is typically full range).
const FRAG: &str = r#"#version 150
in vec2 v_uv;
out vec4 f_color;
uniform sampler2D u_y;
uniform sampler2D u_u;
uniform sampler2D u_v;
uniform int u_nv12;
void main() {
    float y = texture(u_y, v_uv).r;
    vec2 chroma;
    if (u_nv12 != 0) {
        chroma = texture(u_u, v_uv).rg;
    } else {
        chroma = vec2(texture(u_u, v_uv).r, texture(u_v, v_uv).r);
    }
    float cb = chroma.x - 0.5;
    float cr = chroma.y - 0.5;
    float r = y + 1.5748 * cr;
    float g = y - 0.1873 * cb - 0.4681 * cr;
    float b = y + 1.8556 * cb;
    f_color = vec4(clamp(vec3(r, g, b), 0.0, 1.0), 1.0);
}
"#;

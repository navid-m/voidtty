use anyhow::{Context, Result, bail};
use fontdue::{Font, FontSettings};
use gl::types::*;
use std::collections::HashMap;
use std::ffi::CString;

// ── Glyph atlas ───────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
pub struct GlyphMetrics {
    pub u0: f32, pub v0: f32,
    pub u1: f32, pub v1: f32,
    pub bw: f32, pub bh: f32,
    /// Horizontal offset from pen to bitmap left edge.
    pub xoff: f32,
    /// Vertical offset from baseline to bitmap top edge.
    pub yoff: f32,
    pub advance: f32,
}

pub struct GlyphAtlas {
    pub texture:     GLuint,
    pub atlas_w:     u32,
    pub atlas_h:     u32,
    pub cell_w:      f32,
    pub cell_h:      f32,
    pub line_height: f32,
    pub ascent:      f32,
    glyphs: HashMap<char, GlyphMetrics>,
}

impl GlyphAtlas {
    /// Rasterises printable ASCII into a single R8 OpenGL texture.
    pub fn build(font_data: &[u8], face_index: u32, font_size: f32) -> Result<Self> {
        let settings = FontSettings {
            collection_index: face_index,
            ..FontSettings::default()
        };
        let font = Font::from_bytes(font_data, settings)
            .map_err(|e| anyhow::anyhow!("fontdue: {e}"))?;

        let chars: Vec<char> = (0x20u8..=0x7eu8).map(|b| b as char).collect();

        let mut max_w = 0u32;
        let mut max_h = 0u32;
        for &c in &chars {
            let (m, _) = font.rasterize(c, font_size);
            max_w = max_w.max(m.width as u32 + 2);
            max_h = max_h.max(m.height as u32 + 2);
        }

        // Cell width is the advance of a regular glyph, not the widest bitmap:
        // for monospaced fonts this keeps PTY columns aligned with drawn text.
        let advance = font.rasterize('M', font_size).0.advance_width;
        let cell_w = if advance.is_finite() && advance > 0.0 {
            advance.ceil() as u32
        } else {
            max_w
        }
        .max(1);
        // Atlas slots must still fit glyphs that overhang their advance.
        let slot_w = cell_w.max(max_w);
        let cell_h = max_h.max(1);

        let cols: u32 = 16;
        let rows = (chars.len() as u32 + cols - 1) / cols;
        let atlas_w = cols * slot_w;
        let atlas_h = rows * cell_h;

        let mut pixels = vec![0u8; (atlas_w * atlas_h) as usize];
        let mut glyphs: HashMap<char, GlyphMetrics> = HashMap::new();

        // Use 'M' to measure ascent (distance from baseline to top of caps).
        let (ref_m, _) = font.rasterize('M', font_size);
        let ascent = ref_m.height as f32;

        for (i, &c) in chars.iter().enumerate() {
            let (m, bitmap) = font.rasterize(c, font_size);
            let col = i as u32 % cols;
            let row = i as u32 / cols;
            let ox  = col * slot_w;
            let oy  = row * cell_h;

            for y in 0..m.height {
                for x in 0..m.width {
                    let dst = ((oy + y as u32) * atlas_w + ox + x as u32) as usize;
                    pixels[dst] = bitmap[y * m.width + x];
                }
            }

            glyphs.insert(c, GlyphMetrics {
                u0: ox as f32 / atlas_w as f32,
                v0: oy as f32 / atlas_h as f32,
                u1: (ox + m.width as u32)  as f32 / atlas_w as f32,
                v1: (oy + m.height as u32) as f32 / atlas_h as f32,
                bw: m.width as f32,
                bh: m.height as f32,
                xoff: m.xmin as f32,
                yoff: m.ymin as f32,
                advance: m.advance_width,
            });
        }

        let mut texture = 0u32;
        unsafe {
            gl::GenTextures(1, &mut texture);
            gl::BindTexture(gl::TEXTURE_2D, texture);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR as i32);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR as i32);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE as i32);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE as i32);
            gl::PixelStorei(gl::UNPACK_ALIGNMENT, 1);
            gl::TexImage2D(
                gl::TEXTURE_2D, 0, gl::RED as i32,
                atlas_w as i32, atlas_h as i32, 0,
                gl::RED, gl::UNSIGNED_BYTE,
                pixels.as_ptr().cast(),
            );
        }

        Ok(Self {
            texture, atlas_w, atlas_h,
            cell_w: cell_w as f32,
            cell_h: cell_h as f32,
            line_height: cell_h as f32,
            ascent,
            glyphs,
        })
    }

    pub fn get(&self, c: char) -> Option<&GlyphMetrics> {
        self.glyphs.get(&c)
    }
}

// ── Shaders ───────────────────────────────────────────────────────────────────

const VS_TEXT: &str = r#"
#version 330 core
layout(location=0) in vec2 aPos;
layout(location=1) in vec2 aUV;
layout(location=2) in vec4 aCol;
out vec2 vUV;
out vec4 vCol;
uniform vec2 uRes;
void main() {
    vec2 ndc = aPos / uRes * 2.0 - 1.0;
    ndc.y = -ndc.y;
    gl_Position = vec4(ndc, 0.0, 1.0);
    vUV  = aUV;
    vCol = aCol;
}
"#;

const FS_TEXT: &str = r#"
#version 330 core
in vec2 vUV;
in vec4 vCol;
out vec4 fragColor;
uniform sampler2D uAtlas;
void main() {
    float alpha = texture(uAtlas, vUV).r;
    fragColor = vec4(vCol.rgb, vCol.a * alpha);
}
"#;

const VS_RECT: &str = r#"
#version 330 core
layout(location=0) in vec2 aPos;
layout(location=1) in vec4 aCol;
out vec4 vCol;
uniform vec2 uRes;
void main() {
    vec2 ndc = aPos / uRes * 2.0 - 1.0;
    ndc.y = -ndc.y;
    gl_Position = vec4(ndc, 0.0, 1.0);
    vCol = aCol;
}
"#;

const FS_RECT: &str = r#"
#version 330 core
in vec4 vCol;
out vec4 fragColor;
void main() { fragColor = vCol; }
"#;

fn compile_shader(src: &str, kind: GLenum) -> Result<GLuint> {
    unsafe {
        let id = gl::CreateShader(kind);
        let c  = CString::new(src).unwrap();
        gl::ShaderSource(id, 1, &c.as_ptr(), std::ptr::null());
        gl::CompileShader(id);
        let mut ok = 0i32;
        gl::GetShaderiv(id, gl::COMPILE_STATUS, &mut ok);
        if ok == 0 {
            let mut len = 0i32;
            gl::GetShaderiv(id, gl::INFO_LOG_LENGTH, &mut len);
            let mut buf = vec![0u8; len as usize];
            gl::GetShaderInfoLog(id, len, std::ptr::null_mut(), buf.as_mut_ptr().cast());
            bail!("shader: {}", String::from_utf8_lossy(&buf));
        }
        Ok(id)
    }
}

fn link_program(vs: GLuint, fs: GLuint) -> Result<GLuint> {
    unsafe {
        let id = gl::CreateProgram();
        gl::AttachShader(id, vs);
        gl::AttachShader(id, fs);
        gl::LinkProgram(id);
        let mut ok = 0i32;
        gl::GetProgramiv(id, gl::LINK_STATUS, &mut ok);
        if ok == 0 {
            let mut len = 0i32;
            gl::GetProgramiv(id, gl::INFO_LOG_LENGTH, &mut len);
            let mut buf = vec![0u8; len as usize];
            gl::GetProgramInfoLog(id, len, std::ptr::null_mut(), buf.as_mut_ptr().cast());
            bail!("program: {}", String::from_utf8_lossy(&buf));
        }
        gl::DeleteShader(vs);
        gl::DeleteShader(fs);
        Ok(id)
    }
}

// ── Vertex types ──────────────────────────────────────────────────────────────

#[repr(C)]
struct TV { x: f32, y: f32, u: f32, v: f32, r: f32, g: f32, b: f32, a: f32 }

#[repr(C)]
struct RV { x: f32, y: f32, r: f32, g: f32, b: f32, a: f32 }

// ── Renderer ──────────────────────────────────────────────────────────────────

pub struct Renderer {
    prog_text: GLuint,
    prog_rect: GLuint,
    vao_text:  GLuint,
    vbo_text:  GLuint,
    vao_rect:  GLuint,
    vbo_rect:  GLuint,
    pub atlas: GlyphAtlas,
    pub win_w: f32,
    pub win_h: f32,
}

impl Renderer {
    pub fn new(
        font_data: &[u8],
        face_index: u32,
        font_size: f32,
        win_w: u32,
        win_h: u32,
    ) -> Result<Self> {
        let atlas = GlyphAtlas::build(font_data, face_index, font_size)
            .context("building glyph atlas")?;

        let prog_text = link_program(
            compile_shader(VS_TEXT, gl::VERTEX_SHADER)?,
            compile_shader(FS_TEXT, gl::FRAGMENT_SHADER)?,
        )?;
        let prog_rect = link_program(
            compile_shader(VS_RECT, gl::VERTEX_SHADER)?,
            compile_shader(FS_RECT, gl::FRAGMENT_SHADER)?,
        )?;

        let (vao_text, vbo_text) = unsafe { make_vao::<TV>(
            &[(0, 2, 0), (1, 2, 8), (2, 4, 16)],
        )};
        let (vao_rect, vbo_rect) = unsafe { make_vao::<RV>(
            &[(0, 2, 0), (1, 4, 8)],
        )};

        unsafe {
            gl::Enable(gl::BLEND);
            gl::BlendFunc(gl::SRC_ALPHA, gl::ONE_MINUS_SRC_ALPHA);
            gl::Viewport(0, 0, win_w as i32, win_h as i32);
        }

        Ok(Self {
            prog_text, prog_rect,
            vao_text, vbo_text,
            vao_rect, vbo_rect,
            atlas,
            win_w: win_w as f32,
            win_h: win_h as f32,
        })
    }

    pub fn resize(&mut self, w: u32, h: u32) {
        self.win_w = w as f32;
        self.win_h = h as f32;
        unsafe { gl::Viewport(0, 0, w as i32, h as i32); }
    }

    pub fn clear(&self, r: f32, g: f32, b: f32, a: f32) {
        unsafe {
            gl::ClearColor(r, g, b, a);
            gl::Clear(gl::COLOR_BUFFER_BIT);
        }
    }

    /// Draws a filled axis-aligned rectangle in pixel coordinates.
    pub fn draw_rect(&self, x: f32, y: f32, w: f32, h: f32, r: f32, g: f32, b: f32, a: f32) {
        let (x1, y1) = (x + w, y + h);
        let mk = |px: f32, py: f32| RV { x: px, y: py, r, g, b, a };
        let verts = [
            mk(x, y), mk(x1, y), mk(x, y1),
            mk(x1, y), mk(x1, y1), mk(x, y1),
        ];
        unsafe {
            gl::UseProgram(self.prog_rect);
            uniform2f(self.prog_rect, "uRes", self.win_w, self.win_h);
            upload_and_draw(self.vao_rect, self.vbo_rect, &verts, 6);
        }
    }

    /// Draws `text` with the top-left of the first cell at `(px, py)`.
    pub fn draw_text(&self, text: &str, px: f32, py: f32, r: f32, g: f32, b: f32, a: f32) {
        let mut verts: Vec<TV> = Vec::with_capacity(text.len() * 6);
        let mut pen_x = px;
        let baseline = py + self.atlas.ascent;

        for c in text.chars() {
            let ch = if c >= ' ' && c <= '~' { c } else { ' ' };
            let Some(gm) = self.atlas.get(ch) else {
                pen_x += self.atlas.cell_w;
                continue;
            };

            let x0 = pen_x + gm.xoff;
            let y0 = baseline - gm.bh - gm.yoff;
            let x1 = x0 + gm.bw;
            let y1 = y0 + gm.bh;

            for &(x, y, u, v) in &[
                (x0, y0, gm.u0, gm.v0), (x1, y0, gm.u1, gm.v0), (x0, y1, gm.u0, gm.v1),
                (x1, y0, gm.u1, gm.v0), (x1, y1, gm.u1, gm.v1), (x0, y1, gm.u0, gm.v1),
            ] {
                verts.push(TV { x, y, u, v, r, g, b, a });
            }

            pen_x += gm.advance;
        }

        if verts.is_empty() { return; }

        unsafe {
            gl::UseProgram(self.prog_text);
            uniform2f(self.prog_text, "uRes", self.win_w, self.win_h);
            uniform1i(self.prog_text, "uAtlas", 0);
            gl::ActiveTexture(gl::TEXTURE0);
            gl::BindTexture(gl::TEXTURE_2D, self.atlas.texture);
            upload_and_draw(self.vao_text, self.vbo_text, &verts, verts.len() as i32);
        }
    }

    pub fn cell_w(&self)      -> f32 { self.atlas.cell_w }
    pub fn line_height(&self) -> f32 { self.atlas.line_height }
}

// ── GL helpers ────────────────────────────────────────────────────────────────

/// Creates a VAO+VBO pair with the given attribute layout.
/// Each entry is `(location, component_count, byte_offset)`.
unsafe fn make_vao<T>(attribs: &[(u32, i32, usize)]) -> (GLuint, GLuint) {
    let mut vao = 0u32; let mut vbo = 0u32;
    gl::GenVertexArrays(1, &mut vao);
    gl::GenBuffers(1, &mut vbo);
    gl::BindVertexArray(vao);
    gl::BindBuffer(gl::ARRAY_BUFFER, vbo);
    let stride = std::mem::size_of::<T>() as i32;
    for &(loc, count, off) in attribs {
        gl::VertexAttribPointer(loc, count, gl::FLOAT, gl::FALSE, stride, off as *const _);
        gl::EnableVertexAttribArray(loc);
    }
    (vao, vbo)
}

unsafe fn upload_and_draw<T>(vao: GLuint, vbo: GLuint, data: &[T], count: i32) {
    gl::BindVertexArray(vao);
    gl::BindBuffer(gl::ARRAY_BUFFER, vbo);
    gl::BufferData(
        gl::ARRAY_BUFFER,
        (data.len() * std::mem::size_of::<T>()) as isize,
        data.as_ptr().cast(),
        gl::DYNAMIC_DRAW,
    );
    gl::DrawArrays(gl::TRIANGLES, 0, count);
}

unsafe fn uniform2f(prog: GLuint, name: &str, x: f32, y: f32) {
    let cname = CString::new(name).unwrap();
    let loc = gl::GetUniformLocation(prog, cname.as_ptr());
    gl::Uniform2f(loc, x, y);
}

unsafe fn uniform1i(prog: GLuint, name: &str, v: i32) {
    let cname = CString::new(name).unwrap();
    let loc = gl::GetUniformLocation(prog, cname.as_ptr());
    gl::Uniform1i(loc, v);
}

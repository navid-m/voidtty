use anyhow::{Context, Result, bail};
use fontdue::{Font, FontSettings};
use gl::types::*;
use std::collections::{HashMap, HashSet};
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
}

pub struct GlyphAtlas {
    pub texture:     GLuint,
    pub cell_w:      f32,
    pub line_height: f32,
    pub ascent:      f32,
    glyphs: HashMap<u16, GlyphMetrics>,
}

/// One positioned glyph produced by the shaper.
#[derive(Clone, Copy)]
struct ShapedGlyph {
    glyph_id: u16,
    cluster: u32,
    x_offset: f32,
    y_offset: f32,
}

/// Shapes `text` with HarfBuzz-compatible shaping (ligatures enabled), scaling
/// positions from font units into pixels.
fn shape_text(font_data: &[u8], face_index: u32, font_size: f32, text: &str) -> Vec<ShapedGlyph> {
    let Some(face) = rustybuzz::Face::from_slice(font_data, face_index) else {
        return Vec::new();
    };
    let units = face.units_per_em() as f32;
    let scale = if units > 0.0 { font_size / units } else { 1.0 };

    // Ensure programming ligatures are active regardless of the font's
    // default feature set (some fonts put them behind `calt`/`clig`).
    let tag = |b: &[u8; 4]| rustybuzz::ttf_parser::Tag::from_bytes(b);
    let features = [
        rustybuzz::Feature::new(tag(b"liga"), 1, ..),
        rustybuzz::Feature::new(tag(b"clig"), 1, ..),
        rustybuzz::Feature::new(tag(b"calt"), 1, ..),
    ];

    let mut buffer = rustybuzz::UnicodeBuffer::new();
    buffer.push_str(text);
    buffer.guess_segment_properties();
    let shaped = rustybuzz::shape(&face, &features, buffer);

    shaped
        .glyph_infos()
        .iter()
        .zip(shaped.glyph_positions())
        .filter_map(|(info, pos)| {
            Some(ShapedGlyph {
                glyph_id: u16::try_from(info.glyph_id).ok()?,
                cluster: info.cluster,
                x_offset: pos.x_offset as f32 * scale,
                y_offset: pos.y_offset as f32 * scale,
            })
        })
        .collect()
}

impl GlyphAtlas {
    /// Rasterises the given glyph ids into a single R8 OpenGL texture.
    pub fn build(
        font_data: &[u8],
        face_index: u32,
        font_size: f32,
        glyph_ids: &[u16],
    ) -> Result<Self> {
        let settings = FontSettings {
            collection_index: face_index,
            ..FontSettings::default()
        };
        let font = Font::from_bytes(font_data, settings)
            .map_err(|e| anyhow::anyhow!("fontdue: {e}"))?;

        let glyph_count = font.glyph_count();
        let glyph_ids: Vec<u16> = glyph_ids
            .iter()
            .copied()
            .filter(|&id| id < glyph_count)
            .collect();
        let glyph_ids = glyph_ids.as_slice();

        let mut max_w = 0u32;
        let mut max_h = 0u32;
        for &id in glyph_ids {
            let (m, _) = font.rasterize_indexed(id, font_size);
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
        let rows = (glyph_ids.len() as u32 + cols - 1) / cols;
        let atlas_w = (cols * slot_w).max(1);
        let atlas_h = (rows * cell_h).max(1);

        let mut pixels = vec![0u8; (atlas_w * atlas_h) as usize];
        let mut glyphs: HashMap<u16, GlyphMetrics> = HashMap::new();

        // Use 'M' to measure ascent (distance from baseline to top of caps).
        let (ref_m, _) = font.rasterize('M', font_size);
        let ascent = ref_m.height as f32;

        for (i, &id) in glyph_ids.iter().enumerate() {
            let (m, bitmap) = font.rasterize_indexed(id, font_size);
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

            glyphs.insert(id, GlyphMetrics {
                u0: ox as f32 / atlas_w as f32,
                v0: oy as f32 / atlas_h as f32,
                u1: (ox + m.width as u32)  as f32 / atlas_w as f32,
                v1: (oy + m.height as u32) as f32 / atlas_h as f32,
                bw: m.width as f32,
                bh: m.height as f32,
                xoff: m.xmin as f32,
                yoff: m.ymin as f32,
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
            texture,
            cell_w: cell_w as f32,
            line_height: cell_h as f32,
            ascent,
            glyphs,
        })
    }

    pub fn get(&self, id: u16) -> Option<&GlyphMetrics> {
        self.glyphs.get(&id)
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

const VS_ROUND: &str = r#"
#version 330 core
layout(location=0) in vec2 aPos;
layout(location=1) in vec2 aLocal;
layout(location=2) in vec4 aCol;
out vec2 vLocal;
out vec4 vCol;
uniform vec2 uRes;
void main() {
    vec2 ndc = aPos / uRes * 2.0 - 1.0;
    ndc.y = -ndc.y;
    gl_Position = vec4(ndc, 0.0, 1.0);
    vLocal = aLocal;
    vCol = aCol;
}
"#;

const FS_ROUND: &str = r#"
#version 330 core
in vec2 vLocal;
in vec4 vCol;
out vec4 fragColor;
uniform vec2 uHalfSize;
uniform float uRadius;
float sdRoundBox(vec2 p, vec2 b, float r) {
    vec2 q = abs(p) - b + r;
    return length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - r;
}
void main() {
    float d = sdRoundBox(vLocal, uHalfSize, uRadius);
    float aa = max(fwidth(d), 0.0001);
    float coverage = 1.0 - smoothstep(-aa, aa, d);
    fragColor = vec4(vCol.rgb, vCol.a * coverage);
}
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

#[repr(C)]
struct R2V { x: f32, y: f32, lx: f32, ly: f32, r: f32, g: f32, b: f32, a: f32 }

// ── Renderer ──────────────────────────────────────────────────────────────────

pub struct Renderer {
    prog_text: GLuint,
    prog_rect: GLuint,
    prog_round: GLuint,
    vao_text:  GLuint,
    vbo_text:  GLuint,
    vao_rect:  GLuint,
    vbo_rect:  GLuint,
    vao_round: GLuint,
    vbo_round: GLuint,
    pub atlas: GlyphAtlas,
    pub win_w: f32,
    pub win_h: f32,
    pub font_size: f32,
    font_data: Vec<u8>,
    face_index: u32,
    glyph_ids: HashSet<u16>,
}

fn ascii_sample() -> String {
    (0x20u8..=0x7e).map(char::from).collect()
}

impl Renderer {
    pub fn new(
        font_data: &[u8],
        face_index: u32,
        font_size: f32,
        win_w: u32,
        win_h: u32,
    ) -> Result<Self> {
        let glyph_ids: HashSet<u16> = shape_text(font_data, face_index, font_size, &ascii_sample())
            .into_iter()
            .map(|g| g.glyph_id)
            .collect();
        let mut initial: Vec<u16> = glyph_ids.iter().copied().collect();
        initial.sort_unstable();
        let atlas = GlyphAtlas::build(font_data, face_index, font_size, &initial)
            .context("building glyph atlas")?;

        let prog_text = link_program(
            compile_shader(VS_TEXT, gl::VERTEX_SHADER)?,
            compile_shader(FS_TEXT, gl::FRAGMENT_SHADER)?,
        )?;
        let prog_rect = link_program(
            compile_shader(VS_RECT, gl::VERTEX_SHADER)?,
            compile_shader(FS_RECT, gl::FRAGMENT_SHADER)?,
        )?;
        let prog_round = link_program(
            compile_shader(VS_ROUND, gl::VERTEX_SHADER)?,
            compile_shader(FS_ROUND, gl::FRAGMENT_SHADER)?,
        )?;

        let (vao_text, vbo_text) = unsafe { make_vao::<TV>(
            &[(0, 2, 0), (1, 2, 8), (2, 4, 16)],
        )};
        let (vao_rect, vbo_rect) = unsafe { make_vao::<RV>(
            &[(0, 2, 0), (1, 4, 8)],
        )};
        let (vao_round, vbo_round) = unsafe { make_vao::<R2V>(
            &[(0, 2, 0), (1, 2, 8), (2, 4, 16)],
        )};

        unsafe {
            gl::Enable(gl::BLEND);
            gl::BlendFunc(gl::SRC_ALPHA, gl::ONE_MINUS_SRC_ALPHA);
            gl::Viewport(0, 0, win_w as i32, win_h as i32);
        }

        Ok(Self {
            prog_text, prog_rect, prog_round,
            vao_text, vbo_text,
            vao_rect, vbo_rect,
            vao_round, vbo_round,
            atlas,
            win_w: win_w as f32,
            win_h: win_h as f32,
            font_size,
            font_data: font_data.to_vec(),
            face_index,
            glyph_ids,
        })
    }

    fn rebuild_atlas(&mut self) -> Result<()> {
        let mut ids: Vec<u16> = self.glyph_ids.iter().copied().collect();
        ids.sort_unstable();
        let atlas = GlyphAtlas::build(&self.font_data, self.face_index, self.font_size, &ids)
            .context("rebuilding glyph atlas")?;
        let old = std::mem::replace(&mut self.atlas, atlas);
        unsafe { gl::DeleteTextures(1, &old.texture); }
        Ok(())
    }

    /// Rebuilds the glyph atlas at a new size, replacing the old texture.
    pub fn set_font_size(&mut self, size: f32) -> Result<()> {
        if (size - self.font_size).abs() < f32::EPSILON {
            return Ok(());
        }
        self.font_size = size;
        self.rebuild_atlas()
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

    /// Draws a filled rounded rectangle using a signed-distance fragment shader.
    pub fn draw_round_rect(
        &self,
        x: f32, y: f32, w: f32, h: f32, radius: f32,
        r: f32, g: f32, b: f32, a: f32,
    ) {
        let radius = radius.clamp(0.0, w.min(h) * 0.5);
        let (x1, y1) = (x + w, y + h);
        let (cx, cy) = (x + w * 0.5, y + h * 0.5);
        let mk = |px: f32, py: f32| R2V {
            x: px, y: py,
            lx: px - cx, ly: py - cy,
            r, g, b, a,
        };
        let verts = [
            mk(x, y), mk(x1, y), mk(x, y1),
            mk(x1, y), mk(x1, y1), mk(x, y1),
        ];
        unsafe {
            gl::UseProgram(self.prog_round);
            uniform2f(self.prog_round, "uRes", self.win_w, self.win_h);
            uniform2f(self.prog_round, "uHalfSize", w * 0.5, h * 0.5);
            uniform1f(self.prog_round, "uRadius", radius);
            upload_and_draw(self.vao_round, self.vbo_round, &verts, 6);
        }
    }

    /// Draws shaped text with one color per terminal cell. Ligatures are
    /// formed by the shaper and placed at the cell of their starting cluster.
    pub fn draw_text_styled(
        &mut self,
        text: &str,
        colors: &[(f32, f32, f32)],
        px: f32,
        py: f32,
        alpha: f32,
    ) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }

        let shaped = shape_text(&self.font_data, self.face_index, self.font_size, text);
        let mut changed = false;
        for g in &shaped {
            if self.glyph_ids.insert(g.glyph_id) {
                changed = true;
            }
        }
        if changed {
            self.rebuild_atlas()?;
        }

        // Map byte offsets (HarfBuzz clusters) to terminal cell indices.
        let mut byte_to_cell: Vec<(u32, usize)> = Vec::new();
        for (cell_idx, (byte, _)) in text.char_indices().enumerate() {
            byte_to_cell.push((byte as u32, cell_idx));
        }
        let fallback = colors.first().copied().unwrap_or((0.9, 0.9, 0.9));

        let baseline = py + self.atlas.ascent;
        let cell_w = self.atlas.cell_w;
        let mut verts: Vec<TV> = Vec::with_capacity(shaped.len() * 6);

        for sg in &shaped {
            let cell_idx = match byte_to_cell.binary_search_by_key(&sg.cluster, |&(b, _)| b) {
                Ok(i) => byte_to_cell[i].1,
                Err(0) => 0,
                Err(i) => byte_to_cell[i - 1].1,
            };
            let Some(gm) = self.atlas.get(sg.glyph_id) else { continue };
            let (r, g, b) = colors.get(cell_idx).copied().unwrap_or(fallback);

            let x0 = px + cell_idx as f32 * cell_w + sg.x_offset + gm.xoff;
            let y0 = baseline - gm.bh - gm.yoff - sg.y_offset;
            let x1 = x0 + gm.bw;
            let y1 = y0 + gm.bh;

            for &(x, y, u, v) in &[
                (x0, y0, gm.u0, gm.v0), (x1, y0, gm.u1, gm.v0), (x0, y1, gm.u0, gm.v1),
                (x1, y0, gm.u1, gm.v0), (x1, y1, gm.u1, gm.v1), (x0, y1, gm.u0, gm.v1),
            ] {
                verts.push(TV { x, y, u, v, r, g, b, a: alpha });
            }
        }

        if verts.is_empty() {
            return Ok(());
        }

        unsafe {
            gl::UseProgram(self.prog_text);
            uniform2f(self.prog_text, "uRes", self.win_w, self.win_h);
            uniform1i(self.prog_text, "uAtlas", 0);
            gl::ActiveTexture(gl::TEXTURE0);
            gl::BindTexture(gl::TEXTURE_2D, self.atlas.texture);
            upload_and_draw(self.vao_text, self.vbo_text, &verts, verts.len() as i32);
        }
        Ok(())
    }

    pub fn cell_w(&self)      -> f32 { self.atlas.cell_w }
    pub fn line_height(&self) -> f32 { self.atlas.line_height }
}

// ── GL helpers ────────────────────────────────────────────────────────────────

/// Creates a VAO+VBO pair with the given attribute layout.
/// Each entry is `(location, component_count, byte_offset)`.
unsafe fn make_vao<T>(attribs: &[(u32, i32, usize)]) -> (GLuint, GLuint) { unsafe {
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
}}

unsafe fn upload_and_draw<T>(vao: GLuint, vbo: GLuint, data: &[T], count: i32) { unsafe {
    gl::BindVertexArray(vao);
    gl::BindBuffer(gl::ARRAY_BUFFER, vbo);
    gl::BufferData(
        gl::ARRAY_BUFFER,
        (data.len() * std::mem::size_of::<T>()) as isize,
        data.as_ptr().cast(),
        gl::DYNAMIC_DRAW,
    );
    gl::DrawArrays(gl::TRIANGLES, 0, count);
}}

unsafe fn uniform2f(prog: GLuint, name: &str, x: f32, y: f32) { unsafe {
    let cname = CString::new(name).unwrap();
    let loc = gl::GetUniformLocation(prog, cname.as_ptr());
    gl::Uniform2f(loc, x, y);
}}

unsafe fn uniform1i(prog: GLuint, name: &str, v: i32) { unsafe {
    let cname = CString::new(name).unwrap();
    let loc = gl::GetUniformLocation(prog, cname.as_ptr());
    gl::Uniform1i(loc, v);
}}

unsafe fn uniform1f(prog: GLuint, name: &str, v: f32) { unsafe {
    let cname = CString::new(name).unwrap();
    let loc = gl::GetUniformLocation(prog, cname.as_ptr());
    gl::Uniform1f(loc, v);
}}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes_common_ligatures_when_font_supports_them() {
        let Some(font) = crate::font::load("Cascadia Code") else { return };
        for (lig, a, b) in [("->", "-", ">"), ("=>", "=", ">"), ("!=", "!", "=")] {
            let shaped = shape_text(&font.data, font.face_index, 18.0, lig);
            let isolated: Vec<u16> = a
                .chars()
                .chain(b.chars())
                .flat_map(|c| shape_text(&font.data, font.face_index, 18.0, &c.to_string()))
                .map(|g| g.glyph_id)
                .collect();
            let ids: Vec<u16> = shaped.iter().map(|g| g.glyph_id).collect();
            assert_ne!(ids, isolated, "expected {lig:?} to use ligature glyphs");
        }
    }
}

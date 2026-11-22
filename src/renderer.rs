use anyhow::{Context, Result, bail};
use fontdue::{Font, FontSettings};
use gl::types::*;
use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::rc::Rc;

/// Number of shaped lines kept between frames before the cache is dropped.
const LINE_CACHE_CAPACITY: usize = 2048;

// ── Glyph atlas ───────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
pub struct GlyphMetrics {
    pub u0: f32,
    pub v0: f32,
    pub u1: f32,
    pub v1: f32,
    pub bw: f32,
    pub bh: f32,
    /// Horizontal advance reported by the font.
    pub advance: f32,
    /// Horizontal offset from pen to bitmap left edge.
    pub xoff: f32,
    /// Vertical offset from baseline to bitmap top edge.
    pub yoff: f32,
}

pub struct GlyphAtlas {
    pub texture: GLuint,
    pub cell_w: f32,
    pub line_height: f32,
    pub ascent: f32,
    glyphs: HashMap<u16, GlyphMetrics>,
}

/// Fractional rectangle `(x0, y0, x1, y1)` within one terminal cell.
type BlockRect = (f32, f32, f32, f32);

/// Geometry for Unicode block elements. Rasterising these from the font puts
/// their edges on fractional pixel coordinates, which leaves a visible seam
/// between neighbouring cells (and full blocks come up a pixel short). Drawing
/// them as exact cell rectangles keeps text-mode images seamless.
fn block_element_rects(c: char) -> Option<([BlockRect; 2], usize, f32)> {
    let none = (0.0, 0.0, 0.0, 0.0);
    let rects: ([BlockRect; 2], usize, f32) = match c {
        '\u{2580}' => ([(0.0, 0.0, 1.0, 0.5), none], 1, 1.0),
        '\u{2581}' => ([(0.0, 0.875, 1.0, 1.0), none], 1, 1.0),
        '\u{2582}' => ([(0.0, 0.75, 1.0, 1.0), none], 1, 1.0),
        '\u{2583}' => ([(0.0, 0.625, 1.0, 1.0), none], 1, 1.0),
        '\u{2584}' => ([(0.0, 0.5, 1.0, 1.0), none], 1, 1.0),
        '\u{2585}' => ([(0.0, 0.375, 1.0, 1.0), none], 1, 1.0),
        '\u{2586}' => ([(0.0, 0.25, 1.0, 1.0), none], 1, 1.0),
        '\u{2587}' => ([(0.0, 0.125, 1.0, 1.0), none], 1, 1.0),
        '\u{2588}' => ([(0.0, 0.0, 1.0, 1.0), none], 1, 1.0),
        '\u{2589}' => ([(0.0, 0.0, 0.875, 1.0), none], 1, 1.0),
        '\u{258A}' => ([(0.0, 0.0, 0.75, 1.0), none], 1, 1.0),
        '\u{258B}' => ([(0.0, 0.0, 0.625, 1.0), none], 1, 1.0),
        '\u{258C}' => ([(0.0, 0.0, 0.5, 1.0), none], 1, 1.0),
        '\u{258D}' => ([(0.0, 0.0, 0.375, 1.0), none], 1, 1.0),
        '\u{258E}' => ([(0.0, 0.0, 0.25, 1.0), none], 1, 1.0),
        '\u{258F}' => ([(0.0, 0.0, 0.125, 1.0), none], 1, 1.0),
        '\u{2590}' => ([(0.5, 0.0, 1.0, 1.0), none], 1, 1.0),
        '\u{2591}' => ([(0.0, 0.0, 1.0, 1.0), none], 1, 0.25),
        '\u{2592}' => ([(0.0, 0.0, 1.0, 1.0), none], 1, 0.5),
        '\u{2593}' => ([(0.0, 0.0, 1.0, 1.0), none], 1, 0.75),
        '\u{2594}' => ([(0.0, 0.0, 1.0, 0.125), none], 1, 1.0),
        '\u{2595}' => ([(0.875, 0.0, 1.0, 1.0), none], 1, 1.0),
        '\u{2596}' => ([(0.0, 0.5, 0.5, 1.0), none], 1, 1.0),
        '\u{2597}' => ([(0.5, 0.5, 1.0, 1.0), none], 1, 1.0),
        '\u{2598}' => ([(0.0, 0.0, 0.5, 0.5), none], 1, 1.0),
        '\u{2599}' => ([(0.0, 0.0, 0.5, 1.0), (0.5, 0.5, 1.0, 1.0)], 2, 1.0),
        '\u{259A}' => ([(0.0, 0.0, 0.5, 0.5), (0.5, 0.5, 1.0, 1.0)], 2, 1.0),
        '\u{259B}' => ([(0.0, 0.0, 1.0, 0.5), (0.0, 0.5, 0.5, 1.0)], 2, 1.0),
        '\u{259C}' => ([(0.0, 0.0, 1.0, 0.5), (0.5, 0.5, 1.0, 1.0)], 2, 1.0),
        '\u{259D}' => ([(0.5, 0.0, 1.0, 0.5), none], 1, 1.0),
        '\u{259E}' => ([(0.5, 0.0, 1.0, 0.5), (0.0, 0.5, 0.5, 1.0)], 2, 1.0),
        '\u{259F}' => ([(0.5, 0.0, 1.0, 1.0), (0.0, 0.5, 0.5, 1.0)], 2, 1.0),
        _ => return None,
    };
    Some(rects)
}

/// Whether `c` is a block element that should be drawn as cell geometry.
pub fn is_block_element(c: char) -> bool {
    block_element_rects(c).is_some()
}

/// One positioned glyph produced by the shaper.
#[derive(Clone, Copy)]
struct ShapedGlyph {
    glyph_id: u16,
    cluster: u32,
    x_offset: f32,
    y_offset: f32,
}

/// A shaped glyph resolved to the face that can actually draw it.
struct ResolvedGlyph {
    face: usize,
    glyph: u16,
    cell: usize,
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
        let font =
            Font::from_bytes(font_data, settings).map_err(|e| anyhow::anyhow!("fontdue: {e}"))?;

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
        // Use the font's line metrics so glyphs sit correctly inside their cell
        // (important for full-height UI bars such as Neovim's statusline).
        let line_metrics = font.horizontal_line_metrics(font_size);
        // Layout height must not depend on which glyphs happen to be in the
        // atlas, otherwise printing new Unicode glyphs would shift the grid.
        let line_h = line_metrics
            .map(|m| m.new_line_size.ceil() as u32)
            .unwrap_or_else(|| (font_size * 1.2).ceil() as u32)
            .max(1);

        // Atlas slots must still fit glyphs that overhang their advance.
        let slot_w = cell_w.max(max_w);
        let slot_h = max_h.max(line_h).max(1);

        let cols: u32 = 16;
        let rows = (glyph_ids.len() as u32 + cols - 1) / cols;
        let atlas_w = (cols * slot_w).max(1);
        let atlas_h = (rows * slot_h).max(1);

        let mut pixels = vec![0u8; (atlas_w * atlas_h) as usize];
        let mut glyphs: HashMap<u16, GlyphMetrics> = HashMap::new();

        let ascent = line_metrics
            .map(|m| m.ascent.round())
            .unwrap_or_else(|| font.rasterize('M', font_size).0.height as f32);

        for (i, &id) in glyph_ids.iter().enumerate() {
            let (m, bitmap) = font.rasterize_indexed(id, font_size);
            let col = i as u32 % cols;
            let row = i as u32 / cols;
            let ox = col * slot_w;
            let oy = row * slot_h;

            for y in 0..m.height {
                for x in 0..m.width {
                    let dst = ((oy + y as u32) * atlas_w + ox + x as u32) as usize;
                    pixels[dst] = bitmap[y * m.width + x];
                }
            }

            glyphs.insert(
                id,
                GlyphMetrics {
                    u0: ox as f32 / atlas_w as f32,
                    v0: oy as f32 / atlas_h as f32,
                    u1: (ox + m.width as u32) as f32 / atlas_w as f32,
                    v1: (oy + m.height as u32) as f32 / atlas_h as f32,
                    bw: m.width as f32,
                    bh: m.height as f32,
                    advance: m.advance_width,
                    xoff: m.xmin as f32,
                    yoff: m.ymin as f32,
                },
            );
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
                gl::TEXTURE_2D,
                0,
                gl::RED as i32,
                atlas_w as i32,
                atlas_h as i32,
                0,
                gl::RED,
                gl::UNSIGNED_BYTE,
                pixels.as_ptr().cast(),
            );
        }

        Ok(Self {
            texture,
            cell_w: cell_w as f32,
            line_height: line_h as f32,
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
        let c = CString::new(src).unwrap();
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
#[derive(Clone, Copy)]
struct TV {
    x: f32,
    y: f32,
    u: f32,
    v: f32,
    r: f32,
    g: f32,
    b: f32,
    a: f32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RV {
    x: f32,
    y: f32,
    r: f32,
    g: f32,
    b: f32,
    a: f32,
}

#[repr(C)]
struct R2V {
    x: f32,
    y: f32,
    lx: f32,
    ly: f32,
    r: f32,
    g: f32,
    b: f32,
    a: f32,
}

// ── Renderer ──────────────────────────────────────────────────────────────────

/// Cached uniform locations, resolved once at startup.
struct Uniforms {
    text_res: GLint,
    text_atlas: GLint,
    rect_res: GLint,
    round_res: GLint,
    round_half: GLint,
    round_radius: GLint,
}

/// One font face with its own glyph atlas. Index 0 is the primary font; the
/// rest are fallbacks used for characters the primary font lacks.
struct Face {
    data: Vec<u8>,
    face_index: u32,
    atlas: GlyphAtlas,
    glyph_ids: HashSet<u16>,
}

impl Face {
    fn new(data: Vec<u8>, face_index: u32, font_size: f32) -> Result<Self> {
        let atlas =
            GlyphAtlas::build(&data, face_index, font_size, &[]).context("building glyph atlas")?;
        Ok(Self {
            data,
            face_index,
            atlas,
            glyph_ids: HashSet::new(),
        })
    }

    /// Glyph id for `c`, or `None` when this face has no coverage for it.
    fn glyph_for(&self, c: char) -> Option<u16> {
        let face = rustybuzz::Face::from_slice(&self.data, self.face_index)?;
        face.glyph_index(c).map(|g| g.0).filter(|&glyph| glyph != 0)
    }
}

pub struct Renderer {
    prog_text: GLuint,
    prog_rect: GLuint,
    prog_round: GLuint,
    vao_text: GLuint,
    vbo_text: GLuint,
    vao_rect: GLuint,
    vbo_rect: GLuint,
    vao_round: GLuint,
    vbo_round: GLuint,
    uniforms: Uniforms,
    faces: Vec<Face>,
    fallback_source: crate::font::FallbackFonts,
    /// Resolved fallback glyph per character (cached across frames).
    fallback_cache: HashMap<char, Option<(usize, u16)>>,
    pub win_w: f32,
    pub win_h: f32,
    pub font_size: f32,
    /// Shaped lines keyed by their text, so unchanged rows are not reshaped.
    line_cache: HashMap<String, Rc<Vec<ShapedGlyph>>>,
    /// Batching buffers flushed at most once per pane.
    text_verts: Vec<TV>,
    rect_verts: Vec<RV>,
    /// Atlas texture the pending text vertices belong to.
    text_texture: GLuint,
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
        fallback_source: crate::font::FallbackFonts,
    ) -> Result<Self> {
        let glyph_ids: HashSet<u16> = shape_text(font_data, face_index, font_size, &ascii_sample())
            .into_iter()
            .map(|g| g.glyph_id)
            .filter(|&id| id != 0)
            .collect();
        let mut initial: Vec<u16> = glyph_ids.iter().copied().collect();
        initial.sort_unstable();
        let atlas = GlyphAtlas::build(font_data, face_index, font_size, &initial)
            .context("building glyph atlas")?;
        let primary = Face {
            data: font_data.to_vec(),
            face_index,
            atlas,
            glyph_ids,
        };

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

        let (vao_text, vbo_text) = unsafe { make_vao::<TV>(&[(0, 2, 0), (1, 2, 8), (2, 4, 16)]) };
        let (vao_rect, vbo_rect) = unsafe { make_vao::<RV>(&[(0, 2, 0), (1, 4, 8)]) };
        let (vao_round, vbo_round) =
            unsafe { make_vao::<R2V>(&[(0, 2, 0), (1, 2, 8), (2, 4, 16)]) };

        let uniforms = unsafe {
            Uniforms {
                text_res: get_uniform(prog_text, "uRes"),
                text_atlas: get_uniform(prog_text, "uAtlas"),
                rect_res: get_uniform(prog_rect, "uRes"),
                round_res: get_uniform(prog_round, "uRes"),
                round_half: get_uniform(prog_round, "uHalfSize"),
                round_radius: get_uniform(prog_round, "uRadius"),
            }
        };

        unsafe {
            gl::Enable(gl::BLEND);
            gl::BlendFunc(gl::SRC_ALPHA, gl::ONE_MINUS_SRC_ALPHA);
            gl::Viewport(0, 0, win_w as i32, win_h as i32);
        }

        Ok(Self {
            prog_text,
            prog_rect,
            prog_round,
            vao_text,
            vbo_text,
            vao_rect,
            vbo_rect,
            vao_round,
            vbo_round,
            uniforms,
            faces: vec![primary],
            fallback_source,
            fallback_cache: HashMap::new(),
            win_w: win_w as f32,
            win_h: win_h as f32,
            font_size,
            line_cache: HashMap::new(),
            text_verts: Vec::new(),
            rect_verts: Vec::new(),
            text_texture: 0,
        })
    }

    /// Rebuilds one face's atlas after its glyph set changes.
    fn rebuild_face(&mut self, index: usize) -> Result<()> {
        // Pending vertices reference the old texture; draw them first.
        self.flush_text();
        self.text_texture = 0;

        let font_size = self.font_size;
        let face = &mut self.faces[index];
        let mut ids: Vec<u16> = face.glyph_ids.iter().copied().collect();
        ids.sort_unstable();
        let atlas = GlyphAtlas::build(&face.data, face.face_index, font_size, &ids)
            .context("rebuilding glyph atlas")?;
        let old = std::mem::replace(&mut face.atlas, atlas);
        unsafe {
            gl::DeleteTextures(1, &old.texture);
        }
        Ok(())
    }

    /// Rebuilds every glyph atlas at a new size, replacing the old textures.
    pub fn set_font_size(&mut self, size: f32) -> Result<()> {
        if (size - self.font_size).abs() < f32::EPSILON {
            return Ok(());
        }
        self.font_size = size;
        self.line_cache.clear();
        for index in 0..self.faces.len() {
            self.rebuild_face(index)?;
        }
        Ok(())
    }

    /// Returns the face and glyph id for `c`, loading fallback fonts as needed.
    fn fallback_glyph(&mut self, c: char) -> Option<(usize, u16)> {
        if let Some(&cached) = self.fallback_cache.get(&c) {
            return cached;
        }

        for (index, face) in self.faces.iter().enumerate().skip(1) {
            if let Some(glyph) = face.glyph_for(c) {
                self.fallback_cache.insert(c, Some((index, glyph)));
                return Some((index, glyph));
            }
        }

        while let Some(font) = self.fallback_source.next() {
            let Ok(face) = Face::new(font.data, font.face_index, self.font_size) else {
                continue;
            };
            let glyph = face.glyph_for(c);
            let index = self.faces.len();
            self.faces.push(face);
            if let Some(glyph) = glyph {
                self.fallback_cache.insert(c, Some((index, glyph)));
                return Some((index, glyph));
            }
        }

        self.fallback_cache.insert(c, None);
        None
    }

    pub fn resize(&mut self, w: u32, h: u32) {
        self.win_w = w as f32;
        self.win_h = h as f32;
        unsafe {
            gl::Viewport(0, 0, w as i32, h as i32);
        }
    }

    pub fn clear(&self, r: f32, g: f32, b: f32, a: f32) {
        unsafe {
            gl::ClearColor(r, g, b, a);
            gl::Clear(gl::COLOR_BUFFER_BIT);
        }
    }

    /// Starts a new frame, resetting the geometry batches.
    pub fn begin_frame(&mut self) {
        self.rect_verts.clear();
        self.text_verts.clear();
        self.text_texture = 0;
    }

    /// Emits all batched geometry. Rectangles are drawn before text so glyphs
    /// always sit on top of their cell backgrounds.
    pub fn flush(&mut self) {
        self.flush_rects();
        self.flush_text();
    }

    fn flush_rects(&mut self) {
        if self.rect_verts.is_empty() {
            return;
        }
        unsafe {
            gl::UseProgram(self.prog_rect);
            gl::Uniform2f(self.uniforms.rect_res, self.win_w, self.win_h);
            upload_and_draw(
                self.vao_rect,
                self.vbo_rect,
                &self.rect_verts,
                self.rect_verts.len() as i32,
            );
        }
        self.rect_verts.clear();
    }

    fn flush_text(&mut self) {
        if self.text_verts.is_empty() {
            return;
        }
        // The primary atlas is the default when nothing was batch-bound yet.
        let texture = if self.text_texture != 0 {
            self.text_texture
        } else {
            self.faces[0].atlas.texture
        };
        unsafe {
            gl::UseProgram(self.prog_text);
            gl::Uniform2f(self.uniforms.text_res, self.win_w, self.win_h);
            gl::Uniform1i(self.uniforms.text_atlas, 0);
            gl::ActiveTexture(gl::TEXTURE0);
            gl::BindTexture(gl::TEXTURE_2D, texture);
            upload_and_draw(
                self.vao_text,
                self.vbo_text,
                &self.text_verts,
                self.text_verts.len() as i32,
            );
        }
        self.text_verts.clear();
    }

    /// Draws a filled axis-aligned rectangle in pixel coordinates. Geometry is
    /// batched and submitted once per flush.
    pub fn draw_rect(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, g: f32, b: f32, a: f32) {
        let (x1, y1) = (x + w, y + h);
        let mk = |px: f32, py: f32| RV {
            x: px,
            y: py,
            r,
            g,
            b,
            a,
        };
        self.rect_verts.extend_from_slice(&[
            mk(x, y),
            mk(x1, y),
            mk(x, y1),
            mk(x1, y),
            mk(x1, y1),
            mk(x, y1),
        ]);
    }

    /// Draws a filled rounded rectangle using a signed-distance fragment shader.
    pub fn draw_round_rect(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        radius: f32,
        r: f32,
        g: f32,
        b: f32,
        a: f32,
    ) {
        // Flush first so the round rectangle keeps its place in the draw order.
        self.flush();

        let radius = radius.clamp(0.0, w.min(h) * 0.5);
        let (x1, y1) = (x + w, y + h);
        let (cx, cy) = (x + w * 0.5, y + h * 0.5);
        let mk = |px: f32, py: f32| R2V {
            x: px,
            y: py,
            lx: px - cx,
            ly: py - cy,
            r,
            g,
            b,
            a,
        };
        let verts = [
            mk(x, y),
            mk(x1, y),
            mk(x, y1),
            mk(x1, y),
            mk(x1, y1),
            mk(x, y1),
        ];
        unsafe {
            gl::UseProgram(self.prog_round);
            gl::Uniform2f(self.uniforms.round_res, self.win_w, self.win_h);
            gl::Uniform2f(self.uniforms.round_half, w * 0.5, h * 0.5);
            gl::Uniform1f(self.uniforms.round_radius, radius);
            upload_and_draw(self.vao_round, self.vbo_round, &verts, 6);
        }
    }

    /// Shapes a line, reusing a cached result when the same text was shaped
    /// before (the common case while redrawing an idle screen).
    fn shaped_line(&mut self, text: &str) -> Rc<Vec<ShapedGlyph>> {
        if let Some(cached) = self.line_cache.get(text) {
            return Rc::clone(cached);
        }
        let primary = &self.faces[0];
        let shaped = Rc::new(shape_text(
            &primary.data,
            primary.face_index,
            self.font_size,
            text,
        ));
        if self.line_cache.len() >= LINE_CACHE_CAPACITY {
            self.line_cache.clear();
        }
        self.line_cache.insert(text.to_owned(), Rc::clone(&shaped));
        shaped
    }

    /// Fills a Unicode block element as exact cell rectangles, avoiding the
    /// seams left by fractional glyph outlines.
    pub fn draw_block_element(
        &mut self,
        c: char,
        x: f32,
        y: f32,
        color: (f32, f32, f32),
        alpha: f32,
    ) {
        let Some((rects, count, shade)) = block_element_rects(c) else {
            return;
        };
        let w = self.cell_w();
        let h = self.line_height();
        for &(x0, y0, x1, y1) in &rects[..count] {
            if x1 <= x0 || y1 <= y0 {
                continue;
            }
            self.draw_rect(
                x + x0 * w,
                y + y0 * h,
                (x1 - x0) * w,
                (y1 - y0) * h,
                color.0,
                color.1,
                color.2,
                alpha * shade,
            );
        }
    }

    /// Draws shaped text with one color per terminal cell. Ligatures are
    /// formed by the shaper and placed at the cell of their starting cluster.
    /// Characters the primary font lacks are taken from fallback fonts.
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

        // Cell backgrounds for this row precede its glyphs.
        self.flush_rects();

        let shaped = self.shaped_line(text);

        // Map byte offsets (HarfBuzz clusters) to terminal cell indices.
        let byte_to_cell: Vec<(u32, usize)> = text
            .char_indices()
            .enumerate()
            .map(|(cell, (byte, _))| (byte as u32, cell))
            .collect();
        let cell_for =
            |cluster: u32| match byte_to_cell.binary_search_by_key(&cluster, |&(byte, _)| byte) {
                Ok(i) => byte_to_cell[i].1,
                Err(0) => 0,
                Err(i) => byte_to_cell[i - 1].1,
            };

        // Resolve every glyph to the face that can draw it, registering glyph
        // ids that still need rasterising.
        let mut resolved: Vec<ResolvedGlyph> = Vec::with_capacity(shaped.len());
        let mut changed_faces: HashSet<usize> = HashSet::new();
        for sg in shaped.iter() {
            let cell = cell_for(sg.cluster);
            if sg.glyph_id != 0 {
                if self.faces[0].glyph_ids.insert(sg.glyph_id) {
                    changed_faces.insert(0);
                }
                resolved.push(ResolvedGlyph {
                    face: 0,
                    glyph: sg.glyph_id,
                    cell,
                    x_offset: sg.x_offset,
                    y_offset: sg.y_offset,
                });
                continue;
            }

            // Glyph 0 is .notdef: find a fallback face covering this character.
            let Some(ch) = text
                .get(sg.cluster as usize..)
                .and_then(|s| s.chars().next())
            else {
                continue;
            };
            let Some((face, glyph)) = self.fallback_glyph(ch) else {
                continue;
            };
            if self.faces[face].glyph_ids.insert(glyph) {
                changed_faces.insert(face);
            }
            resolved.push(ResolvedGlyph {
                face,
                glyph,
                cell,
                x_offset: 0.0,
                y_offset: 0.0,
            });
        }

        for face in changed_faces {
            self.rebuild_face(face)?;
        }

        let fallback_color = colors.first().copied().unwrap_or((0.9, 0.9, 0.9));
        let baseline = py + self.faces[0].atlas.ascent;
        let cell_w = self.faces[0].atlas.cell_w;
        self.text_verts.reserve(resolved.len() * 6);

        for rg in &resolved {
            let (gm, texture) = {
                let atlas = &self.faces[rg.face].atlas;
                let Some(gm) = atlas.get(rg.glyph).copied() else {
                    continue;
                };
                (gm, atlas.texture)
            };
            if self.text_texture != texture {
                self.flush_text();
                self.text_texture = texture;
            }

            let (r, g, b) = colors.get(rg.cell).copied().unwrap_or(fallback_color);
            let cell_x = px + rg.cell as f32 * cell_w;
            // Snap to the pixel grid: fractional origins make linear filtering
            // blend a whole extra texel row, which looks like a fuzzy outline.
            let x0 = if rg.face == 0 {
                cell_x + rg.x_offset + gm.xoff
            } else {
                // Fallback faces are proportional; centre their advance in the cell.
                cell_x + (cell_w - gm.advance).max(0.0) * 0.5 + gm.xoff
            }
            .round();
            let y0 = (baseline - gm.bh - gm.yoff - rg.y_offset).round();
            let x1 = x0 + gm.bw;
            let y1 = y0 + gm.bh;

            self.text_verts.extend_from_slice(&[
                TV {
                    x: x0,
                    y: y0,
                    u: gm.u0,
                    v: gm.v0,
                    r,
                    g,
                    b,
                    a: alpha,
                },
                TV {
                    x: x1,
                    y: y0,
                    u: gm.u1,
                    v: gm.v0,
                    r,
                    g,
                    b,
                    a: alpha,
                },
                TV {
                    x: x0,
                    y: y1,
                    u: gm.u0,
                    v: gm.v1,
                    r,
                    g,
                    b,
                    a: alpha,
                },
                TV {
                    x: x1,
                    y: y0,
                    u: gm.u1,
                    v: gm.v0,
                    r,
                    g,
                    b,
                    a: alpha,
                },
                TV {
                    x: x1,
                    y: y1,
                    u: gm.u1,
                    v: gm.v1,
                    r,
                    g,
                    b,
                    a: alpha,
                },
                TV {
                    x: x0,
                    y: y1,
                    u: gm.u0,
                    v: gm.v1,
                    r,
                    g,
                    b,
                    a: alpha,
                },
            ]);
        }
        Ok(())
    }

    pub fn cell_w(&self) -> f32 {
        self.faces[0].atlas.cell_w
    }
    pub fn line_height(&self) -> f32 {
        self.faces[0].atlas.line_height
    }
}

// ── GL helpers ────────────────────────────────────────────────────────────────

/// Creates a VAO+VBO pair with the given attribute layout.
/// Each entry is `(location, component_count, byte_offset)`.
unsafe fn make_vao<T>(attribs: &[(u32, i32, usize)]) -> (GLuint, GLuint) {
    unsafe {
        let mut vao = 0u32;
        let mut vbo = 0u32;
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
}

unsafe fn upload_and_draw<T>(vao: GLuint, vbo: GLuint, data: &[T], count: i32) {
    unsafe {
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
}

unsafe fn get_uniform(prog: GLuint, name: &str) -> GLint {
    unsafe {
        let cname = CString::new(name).unwrap();
        gl::GetUniformLocation(prog, cname.as_ptr())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_elements_fill_whole_cells() {
        let Some((rects, count, _)) = block_element_rects('\u{2588}') else {
            panic!("full block should be drawn as geometry");
        };
        assert_eq!(count, 1);
        assert_eq!(rects[0], (0.0, 0.0, 1.0, 1.0));
        assert!(!is_block_element('A'));
    }

    #[test]
    fn shapes_common_ligatures_when_font_supports_them() {
        let Some(font) = crate::font::load("Cascadia Code") else {
            return;
        };
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

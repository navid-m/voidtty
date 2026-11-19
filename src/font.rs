//! System font discovery.
//!
//! voidtty prefers a monospaced font and, when several are installed, favours
//! programming fonts with ligatures (Fira Code, Cascadia Code, …). The bundled
//! font is only used when no suitable system font can be found.

use fontdb::{Database, FaceInfo, ID, Stretch, Style};

/// A font file plus the index of the face to use within it.
pub struct LoadedFont {
    pub data: Vec<u8>,
    pub face_index: u32,
    pub family: String,
}

/// Ligature-capable monospace families, most preferred first.
const LIGATURE_FAMILIES: &[&str] = &[
    "Fira Code",
    "Cascadia Code",
    "JetBrains Mono",
    "Victor Mono",
    "Iosevka",
    "Hack",
    "Source Code Pro",
    "IBM Plex Mono",
    "Monoid",
    "Fantasque Sans Mono",
    "Inconsolata",
];

/// Widely available monospace families used as a fallback.
const MONOSPACE_FAMILIES: &[&str] = &[
    "Cascadia Mono",
    "Consolas",
    "Menlo",
    "Monaco",
    "SF Mono",
    "Ubuntu Mono",
    "Ubuntu Sans Mono",
    "DejaVu Sans Mono",
    "Liberation Mono",
    "Noto Sans Mono",
    "Courier New",
    "FreeMono",
];

/// Symbol-capable families tried, in order, when the primary font lacks a
/// glyph. Most desktop systems have at least one of these installed.
const FALLBACK_FAMILIES: &[&str] = &[
    "DejaVu Sans",
    "FreeSans",
    "Noto Sans Symbols 2",
    "Noto Sans Symbols2",
    "Noto Sans Symbols",
    "Symbola",
    "Noto Sans",
    "Liberation Sans",
    "Segoe UI Symbol",
    "Apple Symbols",
    "Arial Unicode MS",
];

/// Lazily loadable fallback faces for glyphs missing from the primary font.
pub struct FallbackFonts {
    db: Database,
    candidates: Vec<ID>,
    next: usize,
}

impl FallbackFonts {
    /// Resolves candidate family names against the installed system fonts.
    pub fn new(primary_family: &str) -> Self {
        let mut db = Database::new();
        db.load_system_fonts();

        let mut candidates = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for family in FALLBACK_FAMILIES {
            let Some(id) = find_family(&db, family) else {
                continue;
            };
            let same_as_primary = db
                .face(id)
                .is_some_and(|face| family_name(face).eq_ignore_ascii_case(primary_family));
            if !same_as_primary && seen.insert(id) {
                candidates.push(id);
            }
        }

        Self {
            db,
            candidates,
            next: 0,
        }
    }

    /// Loads the next candidate face, if any remain.
    pub fn next(&mut self) -> Option<LoadedFont> {
        while self.next < self.candidates.len() {
            let id = self.candidates[self.next];
            self.next += 1;
            if let Some(font) = load_face(&self.db, id) {
                return Some(font);
            }
        }
        None
    }
}

/// Loads the best available system font.
///
/// `preferred` is a comma-separated list of family names from the config file.
/// If none of them (or no font at all) can be found, `None` is returned and the
/// caller should fall back to the bundled font.
pub fn load(preferred: &str) -> Option<LoadedFont> {
    let mut db = Database::new();
    db.load_system_fonts();

    // Explicit user choice always wins.
    for family in preferred
        .split(',')
        .map(str::trim)
        .filter(|f| !f.is_empty())
    {
        if let Some(font) = find_family(&db, family).and_then(|id| load_face(&db, id)) {
            return Some(font);
        }
    }

    // Next, prefer known monospace families, ligature-capable ones first.
    for family in LIGATURE_FAMILIES.iter().chain(MONOSPACE_FAMILIES) {
        if let Some(font) = find_family(&db, family).and_then(|id| load_face(&db, id)) {
            return Some(font);
        }
    }

    // Finally accept any font the system reports as monospaced.
    find_any_monospace(&db).and_then(|id| load_face(&db, id))
}

/// Finds the best face belonging to a family, compared case-insensitively.
fn find_family(db: &Database, family: &str) -> Option<ID> {
    let mut best: Option<(FaceRank, ID)> = None;
    for face in db.faces() {
        if !face
            .families
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case(family))
        {
            continue;
        }
        let rank = rank(face);
        if best
            .as_ref()
            .map_or(true, |(best_rank, _)| rank < *best_rank)
        {
            best = Some((rank, face.id));
        }
    }
    best.map(|(_, id)| id)
}

fn find_any_monospace(db: &Database) -> Option<ID> {
    let mut faces: Vec<&FaceInfo> = db.faces().filter(|face| face.monospaced).collect();
    faces.sort_by(|a, b| {
        rank(a)
            .cmp(&rank(b))
            .then_with(|| family_name(a).cmp(family_name(b)))
    });
    faces.first().map(|face| face.id)
}

/// Prefers upright, normal-width faces close to regular (400) weight.
type FaceRank = (bool, bool, u16);

fn rank(face: &FaceInfo) -> FaceRank {
    (
        face.style != Style::Normal,
        face.stretch != Stretch::Normal,
        (face.weight.0 as i32 - 400).unsigned_abs() as u16,
    )
}

fn family_name(face: &FaceInfo) -> &str {
    face.families
        .first()
        .map(|(name, _)| name.as_str())
        .unwrap_or(&face.post_script_name)
}

fn load_face(db: &Database, id: ID) -> Option<LoadedFont> {
    let family = db.face(id).map(family_name).unwrap_or("unknown").to_owned();
    let (data, face_index) = db.with_face_data(id, |data, index| (data.to_vec(), index))?;
    Some(LoadedFont {
        data,
        face_index,
        family,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_selected_font_is_usable() {
        // No system fonts is fine: main() falls back to the bundled font.
        let Some(font) = load("") else { return };
        let settings = fontdue::FontSettings {
            collection_index: font.face_index,
            ..Default::default()
        };
        assert!(
            fontdue::Font::from_bytes(font.data.as_slice(), settings).is_ok(),
            "selected font {:?} could not be parsed",
            font.family,
        );
    }
}

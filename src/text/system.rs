//! The lookup of a family among the fonts installed on the system.

use std::sync::{Mutex, OnceLock};

use ttf_parser::Face;

use super::ResolvedFont;
use crate::scene::FontStyle;

fn font_db() -> &'static fontdb::Database {
    static DB: OnceLock<fontdb::Database> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        db
    })
}

/// The system lookups already done. `requests` keeps the answer to each
/// family, weight and style, a miss included, so a node that names a system
/// family does not scan fontdb every frame. `faces` keeps each parsed face by
/// fontdb id, leaked so it has the `'static` lifetime of an embedded face. A
/// process touches few distinct fonts, so the leak is bounded.
#[derive(Default)]
struct SystemCache {
    requests: Vec<(FontRequest, Option<ResolvedFont>)>,
    faces: Vec<(fontdb::ID, ResolvedFont)>,
}

/// A family, a weight and a style, as `resolve` receives them.
type FontRequest = (Box<str>, u16, FontStyle);

fn system_cache() -> &'static Mutex<SystemCache> {
    static CACHE: OnceLock<Mutex<SystemCache>> = OnceLock::new();
    CACHE.get_or_init(Mutex::default)
}

pub(super) fn font(family: &str, weight: u16, style: FontStyle) -> Option<ResolvedFont> {
    // One lock for the lookup and for the insert, so two threads that ask for
    // the same family do not both parse a face and leak it.
    let mut cache = system_cache().lock().ok()?;
    let answered = cache
        .requests
        .iter()
        .find(|((f, w, s), _)| &**f == family && *w == weight && *s == style);
    if let Some((_, font)) = answered {
        return *font;
    }
    let font = query_system_font(&mut cache.faces, family, weight, style);
    cache.requests.push(((family.into(), weight, style), font));
    font
}

fn query_system_font(
    faces: &mut Vec<(fontdb::ID, ResolvedFont)>,
    family: &str,
    weight: u16,
    style: FontStyle,
) -> Option<ResolvedFont> {
    let db = font_db();
    let style_db = match style {
        FontStyle::Normal => fontdb::Style::Normal,
        FontStyle::Italic => fontdb::Style::Italic,
        FontStyle::Oblique => fontdb::Style::Oblique,
    };
    let query = fontdb::Query {
        families: &[fontdb::Family::Name(family)],
        weight: fontdb::Weight(weight),
        stretch: fontdb::Stretch::Normal,
        style: style_db,
    };
    let id = db.query(&query)?;
    if let Some((_, f)) = faces.iter().find(|(c, _)| *c == id) {
        return Some(*f);
    }

    let face_data = db.with_face_data(id, |bytes, index| -> Option<ResolvedFont> {
        // The face borrows the bytes, so both are leaked.
        let static_bytes: &'static [u8] = bytes.to_vec().leak();
        let face = Face::parse(static_bytes, index).ok()?;
        let face_static: &'static Face<'static> = Box::leak(Box::new(face));
        // The name fontdb reports, so a client resolves the same face.
        let canonical = db
            .face(id)
            .and_then(|info| info.families.first())
            .map_or(family, |(n, _)| n.as_str());
        Some(ResolvedFont {
            family: canonical.to_owned().leak(),
            face: face_static,
        })
    })??;

    faces.push((id, face_data));
    Some(face_data)
}

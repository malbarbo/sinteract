//! Wire format for the server / client protocol.
//!
//! `simage::wire` is a thin layer over the Cap'n Proto schema in
//! `schema/frame.capnp`. The generated Rust bindings (committed at
//! [`frame_capnp`]) are kept private — callers go through the high-level
//! [`encode_frame`], [`encode_event`], [`encode_asset`], [`encode_close`],
//! and [`decode`] entry points. They:
//!
//!   * convert between the typed scene ([`crate::scene`], [`crate::event`]) and
//!     the Cap'n Proto messages;
//!   * use `capnp::serialize::write_message` / `read_message`, the
//!     spec-conformant length-prefixed message format every Cap'n Proto
//!     binding (Rust, JS via `capnp-ts`, C++, …) produces and consumes.
//!
//! The schema is the source of truth for evolution. To regenerate, see the
//! comment at the top of `schema/frame.capnp`.

use std::io::Cursor;

use capnp::message::{Builder as MessageBuilder, ReaderOptions};
use capnp::serialize;

use crate::event::{
    InputEvent, KeyEvent, KeyKind, MOD_ALT, MOD_CTRL, MOD_META, MOD_REPEAT, MOD_SHIFT,
};
use crate::scene::{
    BitmapNode, ClipPath, Scene, DrawNode, FillRule, FontStyle, LineCap, LineJoin,
    LinearGradient, Paint, Path, PathStyle, RadialGradient, Rgba, SpreadMode, Stop, TextNode, verb,
};

use crate::frame_capnp::{
    FillRule as WFillRule, FontStyle as WFontStyle, KeyKind as WKeyKind, LineCap as WLineCap,
    LineJoin as WLineJoin, SpreadMode as WSpreadMode, bitmap_node, clip_path as wire_clip_path,
    draw_list, draw_node, input_event, key_event as wire_key_event,
    linear_gradient as wire_linear_gradient, message, paint as wire_paint, path as wire_path,
    path_style as wire_path_style, radial_gradient as wire_radial_gradient, rgba as wire_rgba,
    stop as wire_stop, text_node,
};

/// Stdio-framing magic. Cap'n Proto's own `serialize::write_message` already
/// length-prefixes every message, but `simage::stdio` wraps each payload in
/// `[SIMG][u32 LE len][bytes]` as an extra guard against accidental
/// non-protocol writers on the same pipe.
pub const FILE_IDENTIFIER: [u8; 4] = *b"SIMG";

/// Errors surfaced from [`decode`].
#[derive(Debug)]
pub enum Error {
    /// The buffer could not be parsed (malformed, truncated, or wrong root).
    Parse(capnp::Error),
    /// `Message`'s union discriminant did not match any known variant.
    UnknownVariant(&'static str, u16),
    /// Required nested struct/list was unset.
    MissingField(&'static str),
    /// A `Path` arrived with a verb stream whose argument count does not
    /// match the float coordinates: e.g. 1 cubic verb (6 floats) but only
    /// 4 floats provided.
    PathLengthMismatch { verbs: usize, coords: usize },
    /// A `Path` carried a verb byte we don't know how to consume.
    UnknownVerb(u8),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Parse(e) => write!(f, "parse error: {e}"),
            Error::UnknownVariant(name, tag) => write!(f, "unknown {name} discriminant: {tag}"),
            Error::MissingField(name) => write!(f, "missing required field: {name}"),
            Error::PathLengthMismatch { verbs, coords } => {
                write!(
                    f,
                    "path verbs ({verbs} bytes) and coords ({coords} floats) disagree"
                )
            }
            Error::UnknownVerb(v) => write!(f, "unknown path verb byte: {v}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<capnp::Error> for Error {
    fn from(e: capnp::Error) -> Self {
        Error::Parse(e)
    }
}

impl From<capnp::NotInSchema> for Error {
    fn from(e: capnp::NotInSchema) -> Self {
        Error::UnknownVariant("enum", e.0)
    }
}

impl From<std::str::Utf8Error> for Error {
    fn from(e: std::str::Utf8Error) -> Self {
        Error::Parse(capnp::Error::failed(e.to_string()))
    }
}

/// One decoded message. Mirrors the four arms of the schema's `Message`
/// union, with bytes already converted to typed IR.
#[derive(Clone, Debug)]
pub enum Decoded {
    Asset {
        id: u32,
        blob: Vec<u8>,
        mime: Option<String>,
    },
    Frame(Scene),
    Event(InputEvent),
    Close,
}

// ---------------------------------------------------------------------------
// Encode side
// ---------------------------------------------------------------------------

fn finish(builder: MessageBuilder<capnp::message::HeapAllocator>) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(256);
    serialize::write_message(&mut bytes, &builder).expect("write_message into Vec is infallible");
    bytes
}

/// Encode a draw list as `Message::Frame`.
pub fn encode_frame(dl: &Scene) -> Vec<u8> {
    let mut builder = MessageBuilder::new_default();
    {
        let msg = builder.init_root::<message::Builder>();
        let frame = msg.init_frame();
        write_scene(frame, dl);
    }
    finish(builder)
}

/// Encode an input event as `Message::Event`.
pub fn encode_event(ev: &InputEvent) -> Vec<u8> {
    let mut builder = MessageBuilder::new_default();
    {
        let msg = builder.init_root::<message::Builder>();
        let ev_b = msg.init_event();
        write_input_event(ev_b, ev);
    }
    finish(builder)
}

/// Encode a bitmap upload as `Message::Asset`.
pub fn encode_asset(id: u32, blob: &[u8], mime: Option<&str>) -> Vec<u8> {
    let mut builder = MessageBuilder::new_default();
    {
        let msg = builder.init_root::<message::Builder>();
        let mut asset = msg.init_asset();
        asset.set_id(id);
        asset.set_blob(blob);
        if let Some(m) = mime {
            asset.set_mime(m);
        }
    }
    finish(builder)
}

/// Encode a session close (the union arm has no payload).
pub fn encode_close() -> Vec<u8> {
    let mut builder = MessageBuilder::new_default();
    {
        let mut msg = builder.init_root::<message::Builder>();
        msg.set_session_close(());
    }
    finish(builder)
}

// ---------------------------------------------------------------------------
// Decode side
// ---------------------------------------------------------------------------

/// Decode a buffer produced by one of the `encode_*` functions.
pub fn decode(bytes: &[u8]) -> Result<Decoded, Error> {
    let reader = serialize::read_message(Cursor::new(bytes), ReaderOptions::new())?;
    let msg: message::Reader = reader.get_root()?;
    match msg.which()? {
        message::Asset(a) => {
            let a = a?;
            let blob = a.get_blob()?.to_vec();
            let mime = a.get_mime();
            let mime = match mime {
                Ok(t) => {
                    let s = t.to_str()?.to_owned();
                    if s.is_empty() { None } else { Some(s) }
                }
                Err(_) => None,
            };
            Ok(Decoded::Asset {
                id: a.get_id(),
                blob,
                mime,
            })
        }
        message::Frame(f) => Ok(Decoded::Frame(read_scene(f?)?)),
        message::Event(e) => Ok(Decoded::Event(read_input_event(e?)?)),
        message::SessionClose(()) => Ok(Decoded::Close),
    }
}

// ---------------------------------------------------------------------------
// Enum mappings
// ---------------------------------------------------------------------------

fn line_cap_to_wire(c: LineCap) -> WLineCap {
    match c {
        LineCap::Butt => WLineCap::Butt,
        LineCap::Round => WLineCap::Round,
        LineCap::Square => WLineCap::Square,
    }
}

fn line_cap_from_wire(c: WLineCap) -> LineCap {
    match c {
        WLineCap::Round => LineCap::Round,
        WLineCap::Square => LineCap::Square,
        _ => LineCap::Butt,
    }
}

fn line_join_to_wire(j: LineJoin) -> WLineJoin {
    match j {
        LineJoin::Miter => WLineJoin::Miter,
        LineJoin::Round => WLineJoin::Round,
        LineJoin::Bevel => WLineJoin::Bevel,
    }
}

fn line_join_from_wire(j: WLineJoin) -> LineJoin {
    match j {
        WLineJoin::Round => LineJoin::Round,
        WLineJoin::Bevel => LineJoin::Bevel,
        _ => LineJoin::Miter,
    }
}

fn fill_rule_to_wire(r: FillRule) -> WFillRule {
    match r {
        FillRule::NonZero => WFillRule::NonZero,
        FillRule::EvenOdd => WFillRule::EvenOdd,
    }
}

fn fill_rule_from_wire(r: WFillRule) -> FillRule {
    match r {
        WFillRule::EvenOdd => FillRule::EvenOdd,
        _ => FillRule::NonZero,
    }
}

fn font_style_to_wire(s: FontStyle) -> WFontStyle {
    match s {
        FontStyle::Normal => WFontStyle::Normal,
        FontStyle::Italic => WFontStyle::Italic,
        FontStyle::Oblique => WFontStyle::Oblique,
    }
}

fn font_style_from_wire(s: WFontStyle) -> FontStyle {
    match s {
        WFontStyle::Italic => FontStyle::Italic,
        WFontStyle::Oblique => FontStyle::Oblique,
        _ => FontStyle::Normal,
    }
}

fn spread_to_wire(s: SpreadMode) -> WSpreadMode {
    match s {
        SpreadMode::Pad => WSpreadMode::Pad,
        SpreadMode::Reflect => WSpreadMode::Reflect,
        SpreadMode::Repeat => WSpreadMode::Repeat,
    }
}

fn spread_from_wire(s: WSpreadMode) -> SpreadMode {
    match s {
        WSpreadMode::Reflect => SpreadMode::Reflect,
        WSpreadMode::Repeat => SpreadMode::Repeat,
        _ => SpreadMode::Pad,
    }
}

fn key_kind_to_wire(k: KeyKind) -> WKeyKind {
    match k {
        KeyKind::Press => WKeyKind::Press,
        KeyKind::Down => WKeyKind::Down,
        KeyKind::Up => WKeyKind::Up,
    }
}

fn key_kind_from_wire(k: WKeyKind) -> KeyKind {
    match k {
        WKeyKind::Down => KeyKind::Down,
        WKeyKind::Up => KeyKind::Up,
        _ => KeyKind::Press,
    }
}

// ---------------------------------------------------------------------------
// Struct writers (build into capnp builders)
// ---------------------------------------------------------------------------

fn write_rgba(mut b: wire_rgba::Builder<'_>, c: Rgba) {
    b.set_r(c.r);
    b.set_g(c.g);
    b.set_b(c.b);
    b.set_a(c.a);
}

fn read_rgba(r: wire_rgba::Reader<'_>) -> Rgba {
    Rgba {
        r: r.get_r(),
        g: r.get_g(),
        b: r.get_b(),
        a: r.get_a(),
    }
}

fn write_stop(mut b: wire_stop::Builder<'_>, s: Stop) {
    b.set_offset(s.offset);
    write_rgba(b.reborrow().init_color(), s.color);
}

fn read_stop(r: wire_stop::Reader<'_>) -> Result<Stop, Error> {
    Ok(Stop {
        offset: r.get_offset(),
        color: read_rgba(r.get_color()?),
    })
}

fn write_linear_gradient(mut b: wire_linear_gradient::Builder<'_>, g: &LinearGradient) {
    b.set_x0(g.x0);
    b.set_y0(g.y0);
    b.set_x1(g.x1);
    b.set_y1(g.y1);
    b.set_spread(spread_to_wire(g.spread));
    let mut stops = b.init_stops(g.stops.len() as u32);
    for (i, s) in g.stops.iter().enumerate() {
        write_stop(stops.reborrow().get(i as u32), *s);
    }
}

fn read_linear_gradient(r: wire_linear_gradient::Reader<'_>) -> Result<LinearGradient, Error> {
    let mut stops = Vec::new();
    if r.has_stops() {
        for s in r.get_stops()?.iter() {
            stops.push(read_stop(s)?);
        }
    }
    Ok(LinearGradient {
        x0: r.get_x0(),
        y0: r.get_y0(),
        x1: r.get_x1(),
        y1: r.get_y1(),
        stops,
        spread: spread_from_wire(r.get_spread()?),
    })
}

fn write_radial_gradient(mut b: wire_radial_gradient::Builder<'_>, g: &RadialGradient) {
    b.set_cx(g.cx);
    b.set_cy(g.cy);
    b.set_radius(g.radius);
    b.set_spread(spread_to_wire(g.spread));
    let mut stops = b.init_stops(g.stops.len() as u32);
    for (i, s) in g.stops.iter().enumerate() {
        write_stop(stops.reborrow().get(i as u32), *s);
    }
}

fn read_radial_gradient(r: wire_radial_gradient::Reader<'_>) -> Result<RadialGradient, Error> {
    let mut stops = Vec::new();
    if r.has_stops() {
        for s in r.get_stops()?.iter() {
            stops.push(read_stop(s)?);
        }
    }
    Ok(RadialGradient {
        cx: r.get_cx(),
        cy: r.get_cy(),
        radius: r.get_radius(),
        stops,
        spread: spread_from_wire(r.get_spread()?),
    })
}

fn write_paint(b: wire_paint::Builder<'_>, p: &Paint) {
    match p {
        Paint::Solid(c) => write_rgba(b.init_solid(), *c),
        Paint::Linear(g) => write_linear_gradient(b.init_linear(), g),
        Paint::Radial(g) => write_radial_gradient(b.init_radial(), g),
    }
}

fn read_paint(r: wire_paint::Reader<'_>) -> Result<Paint, Error> {
    use wire_paint::Which;
    Ok(match r.which()? {
        Which::Solid(c) => Paint::Solid(read_rgba(c?)),
        Which::Linear(g) => Paint::Linear(read_linear_gradient(g?)?),
        Which::Radial(g) => Paint::Radial(read_radial_gradient(g?)?),
    })
}

fn write_path_style(mut b: wire_path_style::Builder<'_>, s: &PathStyle) {
    write_paint(b.reborrow().init_fill(), &s.fill);
    write_paint(b.reborrow().init_stroke(), &s.stroke);
    b.set_stroke_width(s.stroke_width);
    b.set_line_cap(line_cap_to_wire(s.line_cap));
    b.set_line_join(line_join_to_wire(s.line_join));
    b.set_fill_rule(fill_rule_to_wire(s.fill_rule));
    b.set_closed(s.closed);
    b.set_miter_limit(s.miter_limit);
    b.set_dash_offset(s.dash_offset);
    if !s.dash_array.is_empty() {
        let mut out = b.init_dash_array(s.dash_array.len() as u32);
        for (i, v) in s.dash_array.iter().enumerate() {
            out.set(i as u32, *v);
        }
    }
}

fn read_path_style(r: wire_path_style::Reader<'_>) -> Result<PathStyle, Error> {
    let dash_array = if r.has_dash_array() {
        r.get_dash_array()?.iter().collect()
    } else {
        Vec::new()
    };
    Ok(PathStyle {
        fill: read_paint(r.get_fill()?)?,
        stroke: read_paint(r.get_stroke()?)?,
        stroke_width: r.get_stroke_width(),
        line_cap: line_cap_from_wire(r.get_line_cap()?),
        line_join: line_join_from_wire(r.get_line_join()?),
        fill_rule: fill_rule_from_wire(r.get_fill_rule()?),
        closed: r.get_closed(),
        miter_limit: r.get_miter_limit(),
        dash_array,
        dash_offset: r.get_dash_offset(),
    })
}

fn write_clip_path(mut b: wire_clip_path::Builder<'_>, c: &ClipPath) {
    b.set_verbs(&c.verbs);
    b.set_fill_rule(fill_rule_to_wire(c.fill_rule));
    let mut out = b.init_coords(c.coords.len() as u32);
    for (i, &v) in c.coords.iter().enumerate() {
        out.set(i as u32, v);
    }
}

fn read_clip_path(r: wire_clip_path::Reader<'_>) -> Result<ClipPath, Error> {
    let verbs = r.get_verbs()?.to_vec();
    let coords: Vec<f32> = r.get_coords()?.iter().collect();
    validate_path_lengths(&verbs, coords.len())?;
    Ok(ClipPath {
        verbs,
        coords,
        fill_rule: fill_rule_from_wire(r.get_fill_rule()?),
    })
}

fn write_bitmap(mut b: bitmap_node::Builder<'_>, n: &BitmapNode) {
    b.set_id(n.id);
    b.set_m0(n.transform[0]);
    b.set_m1(n.transform[1]);
    b.set_m2(n.transform[2]);
    b.set_m3(n.transform[3]);
    b.set_m4(n.transform[4]);
    b.set_m5(n.transform[5]);
}

fn read_bitmap(r: bitmap_node::Reader<'_>) -> BitmapNode {
    BitmapNode {
        id: r.get_id(),
        transform: [
            r.get_m0(),
            r.get_m1(),
            r.get_m2(),
            r.get_m3(),
            r.get_m4(),
            r.get_m5(),
        ],
    }
}

fn write_text_node(mut b: text_node::Builder<'_>, n: &TextNode) {
    write_rgba(b.reborrow().init_fill(), n.fill);
    write_rgba(b.reborrow().init_stroke(), n.stroke);
    b.set_stroke_width(n.stroke_width);
    b.set_m0(n.transform[0]);
    b.set_m1(n.transform[1]);
    b.set_m2(n.transform[2]);
    b.set_m3(n.transform[3]);
    b.set_m4(n.transform[4]);
    b.set_m5(n.transform[5]);
    b.set_size(n.size);
    b.set_family(&*n.family);
    b.set_weight(n.weight);
    b.set_style(font_style_to_wire(n.style));
    b.set_underline(n.underline);
    b.set_text(&*n.text);
}

fn read_text_node(r: text_node::Reader<'_>) -> Result<TextNode, Error> {
    Ok(TextNode {
        fill: read_rgba(r.get_fill()?),
        stroke: read_rgba(r.get_stroke()?),
        stroke_width: r.get_stroke_width(),
        transform: [
            r.get_m0(),
            r.get_m1(),
            r.get_m2(),
            r.get_m3(),
            r.get_m4(),
            r.get_m5(),
        ],
        size: r.get_size(),
        family: r.get_family()?.to_str()?.to_owned(),
        weight: r.get_weight(),
        style: font_style_from_wire(r.get_style()?),
        underline: r.get_underline(),
        text: r.get_text()?.to_str()?.to_owned(),
    })
}

// ---------------------------------------------------------------------------
// Scene <-> wire
// ---------------------------------------------------------------------------

/// Number of floats consumed by each verb. Used both to validate paths on
/// decode and to size the `coords` list on encode.
const fn coords_per_verb(v: u8) -> Option<usize> {
    match v {
        verb::MOVE | verb::LINE => Some(2),
        verb::QUAD => Some(4),
        verb::CUBIC => Some(6),
        _ => None,
    }
}

/// Walk `verbs` and confirm `coords.len()` matches the sum required, and
/// every byte is a known verb. Returns Ok(()) or the appropriate error.
fn validate_path_lengths(verbs: &[u8], coords_len: usize) -> Result<(), Error> {
    let mut needed = 0usize;
    for &v in verbs {
        match coords_per_verb(v) {
            Some(n) => needed += n,
            None => return Err(Error::UnknownVerb(v)),
        }
    }
    if needed != coords_len {
        return Err(Error::PathLengthMismatch {
            verbs: verbs.len(),
            coords: coords_len,
        });
    }
    Ok(())
}

fn write_path_parts(
    mut b: wire_path::Builder<'_>,
    style: &PathStyle,
    verbs: &[u8],
    coords: &[f32],
) {
    write_path_style(b.reborrow().init_style(), style);
    b.set_verbs(verbs);
    let mut out = b.init_coords(coords.len() as u32);
    for (i, &c) in coords.iter().enumerate() {
        out.set(i as u32, c);
    }
}

fn read_path(r: wire_path::Reader<'_>) -> Result<Path, Error> {
    let style = read_path_style(r.get_style()?)?;
    let verbs = r.get_verbs()?.to_vec();
    let coords: Vec<f32> = r.get_coords()?.iter().collect();
    validate_path_lengths(&verbs, coords.len())?;
    Ok(Path {
        style,
        verbs,
        coords,
    })
}

fn write_drawnode(mut b: draw_node::Builder<'_>, node: &DrawNode) {
    match node {
        DrawNode::Path(p) => write_path_parts(b.init_path(), &p.style, &p.verbs, &p.coords),
        DrawNode::ClipPush(c) => write_clip_path(b.init_clip_push(), c),
        DrawNode::ClipPop => b.set_clip_pop(()),
        DrawNode::Text(t) => write_text_node(b.init_text(), t),
        DrawNode::Bitmap(n) => write_bitmap(b.init_bitmap(), n),
    }
}

fn write_scene(mut b: draw_list::Builder<'_>, dl: &Scene) {
    b.set_width(dl.width);
    b.set_height(dl.height);
    let mut nodes = b.init_nodes(dl.nodes.len() as u32);
    for (i, node) in dl.nodes.iter().enumerate() {
        write_drawnode(nodes.reborrow().get(i as u32), node);
    }
}

fn read_drawnode(node: draw_node::Reader<'_>, out: &mut Scene) -> Result<(), Error> {
    use draw_node::Which;
    match node.which()? {
        Which::Path(p) => {
            let path = read_path(p?)?;
            out.push_path(path);
        }
        Which::ClipPush(c) => out.nodes.push(DrawNode::ClipPush(read_clip_path(c?)?)),
        Which::ClipPop(()) => out.nodes.push(DrawNode::ClipPop),
        Which::Text(t) => out.text(read_text_node(t?)?),
        Which::Bitmap(n) => out.bitmap(read_bitmap(n?)),
    }
    Ok(())
}

fn read_scene(r: draw_list::Reader<'_>) -> Result<Scene, Error> {
    let mut out = Scene::new(r.get_width(), r.get_height());
    if r.has_nodes() {
        for node in r.get_nodes()?.iter() {
            read_drawnode(node, &mut out)?;
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// InputEvent <-> wire
// ---------------------------------------------------------------------------

fn write_input_event(mut b: input_event::Builder<'_>, ev: &InputEvent) {
    match ev {
        InputEvent::Key(k) => {
            let mut kb: wire_key_event::Builder = b.init_key();
            kb.set_kind(key_kind_to_wire(k.kind));
            kb.set_key(&*k.key);
            kb.set_modifiers(k.modifiers);
        }
        InputEvent::Vsync => b.set_tick(()),
        InputEvent::Close => b.set_close(()),
    }
}

fn read_input_event(r: input_event::Reader<'_>) -> Result<InputEvent, Error> {
    use input_event::Which;
    match r.which()? {
        Which::Key(k) => {
            let k = k?;
            Ok(InputEvent::Key(KeyEvent {
                kind: key_kind_from_wire(k.get_kind()?),
                key: k.get_key()?.to_str()?.to_owned(),
                modifiers: k.get_modifiers(),
            }))
        }
        Which::Tick(()) => Ok(InputEvent::Vsync),
        Which::Close(()) => Ok(InputEvent::Close),
    }
}

// ---------------------------------------------------------------------------
// Bitmask helpers
// ---------------------------------------------------------------------------

pub fn modifiers(alt: bool, ctrl: bool, shift: bool, meta: bool, repeat: bool) -> u8 {
    let mut m = 0u8;
    if alt {
        m |= MOD_ALT;
    }
    if ctrl {
        m |= MOD_CTRL;
    }
    if shift {
        m |= MOD_SHIFT;
    }
    if meta {
        m |= MOD_META;
    }
    if repeat {
        m |= MOD_REPEAT;
    }
    m
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_scene() -> Scene {
        let mut dl = Scene::new(120.0, 80.0);
        {
            let mut p = dl.begin_path(PathStyle {
                fill: Paint::rgba(10, 20, 30, 0.5),
                stroke: Paint::rgba(200, 0, 0, 1.0),
                stroke_width: 2.5,
                line_cap: LineCap::Round,
                line_join: LineJoin::Bevel,
                fill_rule: FillRule::EvenOdd,
                closed: true,
                ..PathStyle::default()
            });
            p.move_to(0.0, 0.0);
            p.line_to(10.0, 0.0);
            p.quad_to(15.0, 5.0, 20.0, 10.0);
            p.cubic_to(25.0, 5.0, 30.0, 15.0, 35.0, 20.0);
        }
        {
            let mut clip = dl.push_clip_rect(50.0, 50.0, 30.0, 20.0, 15.0, FillRule::EvenOdd);
            clip.text(TextNode {
                fill: Rgba {
                    r: 0,
                    g: 0,
                    b: 0,
                    a: 1.0,
                },
                transform: crate::scene::text_box_affine(
                    "Liberation Sans",
                    700,
                    FontStyle::Italic,
                    12.0,
                    "Olá",
                    60.0,
                    30.0,
                    50.0,
                    14.0,
                    0.0,
                ),
                size: 12.0,
                family: "Liberation Sans".into(),
                weight: 700,
                style: FontStyle::Italic,
                underline: true,
                text: "Olá".into(),
                ..TextNode::default()
            });
            clip.bitmap(BitmapNode {
                id: 7,
                // 64×64 asset, mirrored horizontally, rotated 90°, centred at (70, 40).
                transform: crate::scene::bitmap_box_affine(64, 64, 70.0, 40.0, -32.0, 32.0, 90.0),
            });
        }
        dl
    }

    fn assert_scene_eq(a: &Scene, b: &Scene) {
        assert_eq!(a.width, b.width);
        assert_eq!(a.height, b.height);
        assert_eq!(a.nodes.len(), b.nodes.len(), "node count");
        for (i, (x, y)) in a.nodes.iter().zip(b.nodes.iter()).enumerate() {
            assert_eq!(format!("{x:?}"), format!("{y:?}"), "node {i}");
        }
    }

    #[test]
    fn frame_round_trip_preserves_drawlist() {
        let dl = sample_scene();
        let bytes = encode_frame(&dl);
        match decode(&bytes).expect("decode") {
            Decoded::Frame(d) => assert_scene_eq(&dl, &d),
            other => panic!("expected Frame, got {other:?}"),
        }
    }

    #[test]
    fn empty_drawlist_round_trips() {
        let dl = Scene::new(640.0, 480.0);
        let bytes = encode_frame(&dl);
        match decode(&bytes).unwrap() {
            Decoded::Frame(d) => {
                assert_eq!(d.width, 640.0);
                assert_eq!(d.height, 480.0);
                assert!(d.nodes.is_empty());
            }
            _ => panic!(),
        }
    }

    #[test]
    fn key_event_round_trip() {
        let ev = InputEvent::Key(KeyEvent {
            kind: KeyKind::Down,
            key: "ArrowLeft".into(),
            modifiers: modifiers(false, true, true, false, false),
        });
        let bytes = encode_event(&ev);
        match decode(&bytes).unwrap() {
            Decoded::Event(InputEvent::Key(k)) => {
                assert_eq!(k.kind, KeyKind::Down);
                assert_eq!(k.key, "ArrowLeft");
                assert!(k.ctrl());
                assert!(k.shift());
                assert!(!k.alt());
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn vsync_and_close_events_round_trip() {
        match decode(&encode_event(&InputEvent::Vsync)).unwrap() {
            Decoded::Event(ev) => assert!(ev.is_vsync()),
            _ => panic!(),
        }
        match decode(&encode_event(&InputEvent::Close)).unwrap() {
            Decoded::Event(ev) => assert!(ev.is_close()),
            _ => panic!(),
        }
    }

    #[test]
    fn asset_round_trip_carries_payload() {
        let blob: Vec<u8> = (0u8..=255).collect();
        let bytes = encode_asset(42, &blob, Some("image/png"));
        match decode(&bytes).unwrap() {
            Decoded::Asset {
                id,
                blob: out,
                mime,
            } => {
                assert_eq!(id, 42);
                assert_eq!(out, blob);
                assert_eq!(mime.as_deref(), Some("image/png"));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn close_message_round_trips() {
        let bytes = encode_close();
        assert!(matches!(decode(&bytes).unwrap(), Decoded::Close));
    }

    #[test]
    fn garbage_bytes_fail_to_decode() {
        let r = decode(&[0u8; 4]);
        assert!(r.is_err(), "expected decode error, got {r:?}");
    }

    #[test]
    fn malformed_path_is_rejected() {
        // Hand-build a Path with verbs=[CUBIC] (needs 6 floats) but only
        // 4 coords → decoder must reject with PathLengthMismatch.
        let mut builder = MessageBuilder::new_default();
        {
            let msg = builder.init_root::<message::Builder>();
            let frame = msg.init_frame();
            let mut nodes = frame.init_nodes(1);
            let node = nodes.reborrow().get(0);
            let mut p = node.init_path();
            // style left default
            let _ = p.reborrow().init_style();
            p.set_verbs(&[verb::CUBIC]);
            let mut coords = p.init_coords(4);
            for i in 0..4 {
                coords.set(i, i as f32);
            }
        }
        let bytes = finish(builder);
        let err = decode(&bytes).unwrap_err();
        assert!(
            matches!(
                err,
                Error::PathLengthMismatch {
                    verbs: 1,
                    coords: 4
                }
            ),
            "got {err:?}",
        );
    }

    #[test]
    fn dash_and_miter_round_trip() {
        let mut dl = Scene::new(100.0, 50.0);
        {
            let mut p = dl.begin_path(PathStyle {
                stroke: Paint::rgba(0, 0, 0, 1.0),
                stroke_width: 2.0,
                miter_limit: 7.5,
                dash_array: vec![4.0, 2.0, 1.0],
                dash_offset: 1.5,
                ..PathStyle::default()
            });
            p.move_to(0.0, 0.0);
            p.line_to(50.0, 50.0);
        }
        let bytes = encode_frame(&dl);
        match decode(&bytes).unwrap() {
            Decoded::Frame(d) => {
                let node = d.nodes.first().expect("one node");
                let DrawNode::Path(p) = node else {
                    panic!("expected path");
                };
                assert_eq!(p.style.miter_limit, 7.5);
                assert_eq!(p.style.dash_array, vec![4.0, 2.0, 1.0]);
                assert_eq!(p.style.dash_offset, 1.5);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn linear_gradient_paint_round_trips() {
        let mut dl = Scene::new(50.0, 50.0);
        let gradient = LinearGradient {
            x0: 0.0,
            y0: 0.0,
            x1: 50.0,
            y1: 0.0,
            stops: vec![
                Stop {
                    offset: 0.0,
                    color: Rgba {
                        r: 255,
                        g: 0,
                        b: 0,
                        a: 1.0,
                    },
                },
                Stop {
                    offset: 0.5,
                    color: Rgba {
                        r: 0,
                        g: 255,
                        b: 0,
                        a: 0.8,
                    },
                },
                Stop {
                    offset: 1.0,
                    color: Rgba {
                        r: 0,
                        g: 0,
                        b: 255,
                        a: 1.0,
                    },
                },
            ],
            ..LinearGradient::default()
        };
        {
            let mut p = dl.begin_path(PathStyle {
                fill: Paint::Linear(gradient.clone()),
                ..PathStyle::default()
            });
            p.move_to(0.0, 0.0);
            p.line_to(50.0, 0.0);
            p.line_to(50.0, 50.0);
        }
        let bytes = encode_frame(&dl);
        match decode(&bytes).unwrap() {
            Decoded::Frame(d) => {
                let DrawNode::Path(p) = d.nodes.first().unwrap() else {
                    panic!();
                };
                assert_eq!(p.style.fill, Paint::Linear(gradient));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn radial_gradient_paint_round_trips() {
        let mut dl = Scene::new(50.0, 50.0);
        let gradient = RadialGradient {
            cx: 25.0,
            cy: 25.0,
            radius: 20.0,
            stops: vec![
                Stop {
                    offset: 0.0,
                    color: Rgba {
                        r: 255,
                        g: 255,
                        b: 255,
                        a: 1.0,
                    },
                },
                Stop {
                    offset: 1.0,
                    color: Rgba {
                        r: 0,
                        g: 0,
                        b: 0,
                        a: 0.0,
                    },
                },
            ],
            ..RadialGradient::default()
        };
        {
            let mut p = dl.begin_path(PathStyle {
                fill: Paint::Radial(gradient.clone()),
                ..PathStyle::default()
            });
            p.move_to(0.0, 0.0);
            p.line_to(50.0, 50.0);
        }
        let bytes = encode_frame(&dl);
        match decode(&bytes).unwrap() {
            Decoded::Frame(d) => {
                let DrawNode::Path(p) = d.nodes.first().unwrap() else {
                    panic!();
                };
                assert_eq!(p.style.fill, Paint::Radial(gradient));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn gradient_spread_mode_round_trips() {
        // Linear with Reflect and Radial with Repeat — both should survive
        // a wire round-trip.
        let mut dl = Scene::new(50.0, 50.0);
        let linear = LinearGradient {
            x0: 0.0,
            y0: 0.0,
            x1: 25.0,
            y1: 0.0,
            spread: SpreadMode::Reflect,
            stops: vec![
                Stop {
                    offset: 0.0,
                    color: Rgba {
                        r: 255,
                        g: 0,
                        b: 0,
                        a: 1.0,
                    },
                },
                Stop {
                    offset: 1.0,
                    color: Rgba {
                        r: 0,
                        g: 0,
                        b: 255,
                        a: 1.0,
                    },
                },
            ],
        };
        {
            let mut p = dl.begin_path(PathStyle {
                fill: Paint::Linear(linear.clone()),
                ..PathStyle::default()
            });
            p.move_to(0.0, 0.0);
            p.line_to(50.0, 50.0);
        }
        let radial = RadialGradient {
            cx: 25.0,
            cy: 25.0,
            radius: 10.0,
            spread: SpreadMode::Repeat,
            stops: vec![
                Stop {
                    offset: 0.0,
                    color: Rgba {
                        r: 0,
                        g: 255,
                        b: 0,
                        a: 1.0,
                    },
                },
                Stop {
                    offset: 1.0,
                    color: Rgba::default(),
                },
            ],
        };
        {
            let mut p = dl.begin_path(PathStyle {
                fill: Paint::Radial(radial.clone()),
                ..PathStyle::default()
            });
            p.move_to(0.0, 0.0);
            p.line_to(50.0, 50.0);
        }
        let bytes = encode_frame(&dl);
        match decode(&bytes).unwrap() {
            Decoded::Frame(d) => {
                let DrawNode::Path(p0) = &d.nodes[0] else {
                    panic!();
                };
                assert_eq!(p0.style.fill, Paint::Linear(linear));
                let DrawNode::Path(p1) = &d.nodes[1] else {
                    panic!();
                };
                assert_eq!(p1.style.fill, Paint::Radial(radial));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn clip_path_round_trips() {
        // Build a clip path directly (not via clip_rect) with a quadratic
        // segment + even-odd rule to exercise verb walking and fill-rule
        // preservation across the wire.
        let mut dl = Scene::new(50.0, 50.0);
        drop(dl.push_clip(ClipPath {
            verbs: vec![verb::MOVE, verb::LINE, verb::QUAD, verb::LINE],
            coords: vec![0.0, 0.0, 30.0, 0.0, 40.0, 25.0, 30.0, 40.0, 0.0, 40.0],
            fill_rule: FillRule::EvenOdd,
        }));
        let bytes = encode_frame(&dl);
        match decode(&bytes).unwrap() {
            Decoded::Frame(d) => {
                let DrawNode::ClipPush(c) = &d.nodes[0] else {
                    panic!("expected ClipPush, got {:?}", d.nodes[0]);
                };
                assert_eq!(
                    c.verbs,
                    vec![verb::MOVE, verb::LINE, verb::QUAD, verb::LINE]
                );
                assert_eq!(
                    c.coords,
                    vec![0.0, 0.0, 30.0, 0.0, 40.0, 25.0, 30.0, 40.0, 0.0, 40.0]
                );
                assert_eq!(c.fill_rule, FillRule::EvenOdd);
                assert!(matches!(&d.nodes[1], DrawNode::ClipPop));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn malformed_clip_path_is_rejected() {
        // Hand-build a ClipPath with CUBIC (needs 6 floats) and only 2 coords;
        // decoder must reject it the same way it rejects malformed Paths.
        let mut builder = MessageBuilder::new_default();
        {
            let msg = builder.init_root::<message::Builder>();
            let frame = msg.init_frame();
            let mut nodes = frame.init_nodes(1);
            let node = nodes.reborrow().get(0);
            let mut c = node.init_clip_push();
            c.set_verbs(&[verb::CUBIC]);
            let mut coords = c.init_coords(2);
            coords.set(0, 0.0);
            coords.set(1, 0.0);
        }
        let bytes = finish(builder);
        let err = decode(&bytes).unwrap_err();
        assert!(
            matches!(
                err,
                Error::PathLengthMismatch {
                    verbs: 1,
                    coords: 2
                }
            ),
            "got {err:?}",
        );
    }

    #[test]
    fn default_style_uses_solid_transparent_paint() {
        // Encode/decode with all defaults: paint should be Solid(transparent),
        // dash empty, miter_limit at SVG default. Guards against future
        // accidental changes to Default.
        let mut dl = Scene::new(10.0, 10.0);
        {
            let mut p = dl.begin_path(PathStyle::default());
            p.move_to(0.0, 0.0);
        }
        let bytes = encode_frame(&dl);
        match decode(&bytes).unwrap() {
            Decoded::Frame(d) => {
                let DrawNode::Path(p) = d.nodes.first().unwrap() else {
                    panic!();
                };
                assert_eq!(p.style.fill, Paint::Solid(Rgba::default()));
                assert_eq!(p.style.stroke, Paint::Solid(Rgba::default()));
                assert!(p.style.dash_array.is_empty());
                assert_eq!(p.style.dash_offset, 0.0);
                assert_eq!(p.style.miter_limit, crate::scene::DEFAULT_MITER_LIMIT);
            }
            _ => panic!(),
        }
    }
}

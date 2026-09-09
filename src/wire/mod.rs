//! Codec for the server and client protocol.
//!
//! The generated bindings in `src/wire/frame_capnp.rs` stay private. A
//! caller goes through [`encode_frame`], [`encode_event`], [`encode_asset`],
//! [`encode_close`] and [`decode`], which convert between the types of
//! [`crate::scene`] and [`crate::event`] and the Cap'n Proto messages. The
//! bytes are the standard `serialize::write_message` format, so every Cap'n
//! Proto binding reads them.
//!
//! The schema in `schema/frame.capnp` is the source of truth, and its header
//! says how to regenerate the bindings.

use std::io::Cursor;

use capnp::message::{Builder as MessageBuilder, ReaderOptions};
use capnp::serialize;

use crate::event::{
    InputEvent, KeyEvent, KeyKind, MOD_ALT, MOD_CTRL, MOD_META, MOD_REPEAT, MOD_SHIFT,
};
use crate::renderer::AllocError;
use crate::renderer::sealed::Paint as PaintSink;
use crate::scene::{
    Bitmap, ClipPath, Dash, Element, FillRule, FontStyle, Gradient, GradientGeom, LineCap,
    LineJoin, Paint, Path, PathStyle, Rgba, Scene, Segment, SegmentKind, Segments, SpreadMode,
    Stop, TextNode,
};

use crate::frame_capnp::{
    FillRule as WFillRule, FontStyle as WFontStyle, KeyKind as WKeyKind, LineCap as WLineCap,
    LineJoin as WLineJoin, SpreadMode as WSpreadMode, bitmap_node, clip_path as wire_clip_path,
    clipped as wire_clipped, element, input_event, key_event as wire_key_event, message,
    paint as wire_paint, path as wire_path, path_style as wire_path_style, rgba as wire_rgba,
    scene as wire_scene, stop as wire_stop, text_node,
};

/// Magic of the stdio framing. Cap'n Proto already length-prefixes each
/// message, and `sinteract::stdio` wraps the payload in `[SINT][u32 LE len]` as
/// well, so that text from another writer on the same pipe is rejected before
/// it reaches the Cap'n Proto reader.
pub const FILE_IDENTIFIER: [u8; 4] = *b"SINT";

/// Errors surfaced from [`decode`].
#[derive(Debug)]
pub enum Error {
    /// Cap'n Proto rejected the bytes as malformed, truncated, or of the
    /// wrong root.
    Parse(capnp::Error),
    /// A union discriminant matches no known variant.
    UnknownVariant(&'static str, u16),
    /// A required nested struct or list is unset.
    MissingField(&'static str),
    /// The verbs of a `Path` claim a number of floats that its coords do not
    /// hold.
    PathLengthMismatch { verbs: usize, coords: usize },
    /// A `Path` carries a verb byte this crate does not know.
    UnknownVerb(u8),
    /// The stream decoder saw a `Message` that is not a `Frame`.
    WrongMessageKind,
    /// The renderer of a streamed frame could not size its surface.
    Surface(AllocError),
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
            Error::WrongMessageKind => {
                write!(f, "expected Message::Frame, got a different union arm")
            }
            Error::Surface(e) => e.fmt(f),
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

impl From<AllocError> for Error {
    fn from(e: AllocError) -> Self {
        Error::Surface(e)
    }
}

impl From<std::str::Utf8Error> for Error {
    fn from(e: std::str::Utf8Error) -> Self {
        Error::Parse(capnp::Error::failed(e.to_string()))
    }
}

/// One decoded message, one variant per arm of the `Message` union.
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

/// Encode a scene as `Message::Frame`.
pub fn encode_frame(scene: &Scene) -> Vec<u8> {
    let mut builder = MessageBuilder::new_default();
    {
        let msg = builder.init_root::<message::Builder>();
        let frame = msg.init_frame();
        write_scene(frame, scene);
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

/// Encode a session close.
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

fn write_stops(mut b: capnp::struct_list::Builder<'_, wire_stop::Owned>, stops: &[Stop]) {
    for (i, s) in stops.iter().enumerate() {
        write_stop(b.reborrow().get(i as u32), *s);
    }
}

fn read_stops(r: capnp::struct_list::Reader<'_, wire_stop::Owned>) -> Result<Vec<Stop>, Error> {
    r.iter().map(read_stop).collect()
}

fn write_paint(b: wire_paint::Builder<'_>, p: &Paint) {
    match p {
        Paint::Solid(c) => write_rgba(b.init_solid(), *c),
        Paint::Gradient(g) => match g.geom {
            GradientGeom::Linear { x0, y0, x1, y1 } => {
                let mut b = b.init_linear();
                b.set_x0(x0);
                b.set_y0(y0);
                b.set_x1(x1);
                b.set_y1(y1);
                b.set_spread(spread_to_wire(g.spread));
                write_stops(b.init_stops(g.stops.len() as u32), &g.stops);
            }
            GradientGeom::Radial { cx, cy, radius } => {
                let mut b = b.init_radial();
                b.set_cx(cx);
                b.set_cy(cy);
                b.set_radius(radius);
                b.set_spread(spread_to_wire(g.spread));
                write_stops(b.init_stops(g.stops.len() as u32), &g.stops);
            }
        },
    }
}

fn read_paint(r: wire_paint::Reader<'_>) -> Result<Paint, Error> {
    use wire_paint::Which;
    // A null stops pointer reads as an empty list, so an absent ramp needs
    // no guard.
    let (geom, stops, spread) = match r.which()? {
        Which::Solid(c) => return Ok(Paint::Solid(read_rgba(c?))),
        Which::Linear(g) => {
            let g = g?;
            let geom = GradientGeom::Linear {
                x0: g.get_x0(),
                y0: g.get_y0(),
                x1: g.get_x1(),
                y1: g.get_y1(),
            };
            (geom, read_stops(g.get_stops()?)?, g.get_spread()?)
        }
        Which::Radial(g) => {
            let g = g?;
            let geom = GradientGeom::Radial {
                cx: g.get_cx(),
                cy: g.get_cy(),
                radius: g.get_radius(),
            };
            (geom, read_stops(g.get_stops()?)?, g.get_spread()?)
        }
    };
    Ok(Paint::gradient(Gradient {
        geom,
        stops,
        spread: spread_from_wire(spread),
    }))
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
    if let Some(dash) = &s.dash {
        b.set_dash_offset(dash.offset());
        let mut out = b.init_dash_array(dash.array().len() as u32);
        for (i, v) in dash.array().iter().enumerate() {
            out.set(i as u32, *v);
        }
    }
}

fn read_path_style(r: wire_path_style::Reader<'_>) -> Result<PathStyle, Error> {
    // An empty array is a solid stroke, so Dash::new returns None and drops
    // any stray offset.
    let dash = Dash::new(
        r.get_dash_array()?.iter().collect::<Vec<_>>(),
        r.get_dash_offset(),
    )
    .map(Box::new);
    Ok(PathStyle {
        fill: read_paint(r.get_fill()?)?,
        stroke: read_paint(r.get_stroke()?)?,
        stroke_width: r.get_stroke_width(),
        line_cap: line_cap_from_wire(r.get_line_cap()?),
        line_join: line_join_from_wire(r.get_line_join()?),
        fill_rule: fill_rule_from_wire(r.get_fill_rule()?),
        closed: r.get_closed(),
        miter_limit: r.get_miter_limit(),
        dash,
    })
}

fn write_clip_path(mut b: wire_clip_path::Builder<'_>, c: &ClipPath) {
    b.set_fill_rule(fill_rule_to_wire(c.fill_rule));
    write_verbs(
        b.reborrow().init_verbs(c.segments().len() as u32),
        c.segments(),
    );
    write_coords(&mut b.init_coords(coord_count(c.segments())), c.segments());
}

fn read_clip_path(r: wire_clip_path::Reader<'_>) -> Result<ClipPath, Error> {
    let mut clip = ClipPath::default();
    clip.fill_rule = fill_rule_from_wire(r.get_fill_rule()?);
    read_segments(clip.segments_mut(), r.get_verbs()?, r.get_coords()?)?;
    Ok(clip)
}

fn write_bitmap(mut b: bitmap_node::Builder<'_>, n: &Bitmap) {
    b.set_id(n.id);
    b.set_m0(n.transform[0]);
    b.set_m1(n.transform[1]);
    b.set_m2(n.transform[2]);
    b.set_m3(n.transform[3]);
    b.set_m4(n.transform[4]);
    b.set_m5(n.transform[5]);
}

fn read_bitmap(r: bitmap_node::Reader<'_>) -> Bitmap {
    Bitmap {
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
        family: r.get_family()?.to_str()?.into(),
        weight: r.get_weight(),
        style: font_style_from_wire(r.get_style()?),
        underline: r.get_underline(),
        text: r.get_text()?.to_str()?.to_owned(),
    })
}

// ---------------------------------------------------------------------------
// Scene <-> wire
// ---------------------------------------------------------------------------

// A `Path` holds typed segments. The flat verb and coord pair exists only on
// the wire, and the functions below are the only place where the two forms
// meet.

/// Number of floats that `segs` take on the wire.
fn coord_count(segs: Segments<'_>) -> u32 {
    segs.map(|s| s.kind().coords() as u32).sum()
}

/// A `SegmentKind` discriminant is the wire byte, so a verb is a cast. The
/// coords go in a second pass because a capnp builder hands out one field at
/// a time.
fn write_verbs(out: capnp::data::Builder<'_>, segs: Segments<'_>) {
    for (dst, seg) in out.iter_mut().zip(segs) {
        *dst = seg.kind() as u8;
    }
}

/// Write the coordinates in verb order, the layout that [`read_segments`] reads.
fn write_coords(out: &mut capnp::primitive_list::Builder<'_, f32>, segs: Segments<'_>) {
    let mut i = 0;
    for seg in segs {
        for &c in &seg.wire_coords()[..seg.kind().coords()] {
            out.set(i, c);
            i += 1;
        }
    }
}

/// Rebuild the segments into `out`, reusing its allocation. Rejects an
/// unknown verb byte and a coord stream that does not hold exactly what the
/// verbs claim.
fn read_segments(
    out: &mut Vec<Segment>,
    verbs: &[u8],
    coords: capnp::primitive_list::Reader<'_, f32>,
) -> Result<(), Error> {
    let mismatch = || Error::PathLengthMismatch {
        verbs: verbs.len(),
        coords: coords.len() as usize,
    };
    out.clear();
    out.reserve(verbs.len());
    let mut i = 0;
    for &b in verbs {
        let kind = SegmentKind::from_u8(b).ok_or(Error::UnknownVerb(b))?;
        if i + kind.coords() as u32 > coords.len() {
            return Err(mismatch());
        }
        let c = |k: u32| coords.get(i + k);
        out.push(match kind {
            SegmentKind::Move => Segment::Move { x: c(0), y: c(1) },
            SegmentKind::Line => Segment::Line { x: c(0), y: c(1) },
            SegmentKind::Quad => Segment::Quad {
                cx: c(0),
                cy: c(1),
                x: c(2),
                y: c(3),
            },
            SegmentKind::Cubic => Segment::Cubic {
                c1x: c(0),
                c1y: c(1),
                c2x: c(2),
                c2y: c(3),
                x: c(4),
                y: c(5),
            },
        });
        i += kind.coords() as u32;
    }
    // Coords that no verb claims are a mismatch too.
    if i != coords.len() {
        return Err(mismatch());
    }
    Ok(())
}

fn write_path(mut b: wire_path::Builder<'_>, p: &Path) {
    write_path_style(b.reborrow().init_style(), &p.style);
    write_verbs(
        b.reborrow().init_verbs(p.segments().len() as u32),
        p.segments(),
    );
    write_coords(&mut b.init_coords(coord_count(p.segments())), p.segments());
}

/// Decode into `path`, reusing its allocations. The streaming decoder keeps
/// one for a whole frame.
fn read_path_into(r: wire_path::Reader<'_>, path: &mut Path) -> Result<(), Error> {
    path.style = read_path_style(r.get_style()?)?;
    read_segments(path.segments_mut(), r.get_verbs()?, r.get_coords()?)
}

fn read_path(r: wire_path::Reader<'_>) -> Result<Path, Error> {
    let mut path = Path::default();
    read_path_into(r, &mut path)?;
    Ok(path)
}

fn write_element(b: element::Builder<'_>, node: &Element) {
    match node {
        Element::Path(p) => write_path(b.init_path(), p),
        Element::Clipped { clip, elements } => write_clipped(b.init_clipped(), clip, elements),
        Element::Text(t) => write_text_node(b.init_text(), t),
        Element::Bitmap(n) => write_bitmap(b.init_bitmap(), n),
    }
}

fn write_clipped(mut b: wire_clipped::Builder<'_>, clip: &ClipPath, elements: &[Element]) {
    write_clip_path(b.reborrow().init_clip(), clip);
    write_element_list(b.init_elements(elements.len() as u32), elements);
}

fn write_element_list(
    mut list: capnp::struct_list::Builder<'_, element::Owned>,
    elements: &[Element],
) {
    for (i, node) in elements.iter().enumerate() {
        write_element(list.reborrow().get(i as u32), node);
    }
}

fn write_scene(mut b: wire_scene::Builder<'_>, scene: &Scene) {
    b.set_width(scene.width);
    b.set_height(scene.height);
    write_element_list(
        b.init_elements(scene.elements.len() as u32),
        &scene.elements,
    );
}

fn read_element(node: element::Reader<'_>) -> Result<Element, Error> {
    use element::Which;
    Ok(match node.which()? {
        Which::Path(p) => Element::Path(read_path(p?)?),
        Which::Clipped(c) => {
            let c = c?;
            let clip = read_clip_path(c.get_clip()?)?;
            let elements = read_element_list(c.get_elements()?)?;
            Element::Clipped { clip, elements }
        }
        Which::Text(t) => Element::Text(read_text_node(t?)?),
        Which::Bitmap(n) => Element::Bitmap(read_bitmap(n?)),
    })
}

fn read_element_list(
    list: capnp::struct_list::Reader<'_, element::Owned>,
) -> Result<Vec<Element>, Error> {
    list.iter().map(read_element).collect()
}

fn read_scene(r: wire_scene::Reader<'_>) -> Result<Scene, Error> {
    let mut out = Scene::new(r.get_width(), r.get_height());
    if r.has_elements() {
        out.elements = read_element_list(r.get_elements()?)?;
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
// Streaming entry point
// ---------------------------------------------------------------------------

/// Decode one `Message::Frame` from `reader` and paint it onto `paint`. The
/// reader is walked lazily, so the element list never becomes a
/// `Vec<Element>`, and a `Clipped` subtree recurses through
/// [`Paint::with_clip`]. Every path decodes into one scratch [`Path`] that
/// the whole frame reuses, so decoding allocates about as much as the
/// longest path.
///
/// The surface is sized once, after the dimensions are known and before any
/// element is painted. Any other message returns [`Error::WrongMessageKind`].
/// Use [`decode`] for those.
pub(crate) fn stream_frame<P: PaintSink, R: std::io::Read>(
    paint: &mut P,
    reader: R,
) -> Result<(), Error> {
    let msg = serialize::read_message(reader, ReaderOptions::new())?;
    let m: message::Reader = msg.get_root()?;
    match m.which()? {
        message::Frame(f) => {
            let frame = f?;
            paint.ensure_size(frame.get_width(), frame.get_height())?;
            if frame.has_elements() {
                let mut scratch = Path::default();
                stream_elements(paint, frame.get_elements()?, &mut scratch)?;
            }
            Ok(())
        }
        message::Asset(_) | message::Event(_) | message::SessionClose(()) => {
            Err(Error::WrongMessageKind)
        }
    }
}

/// `scratch` is the one [`Path`] of the frame. A clip gets its own, because
/// it is still live while its children decode.
fn stream_elements<P: PaintSink>(
    paint: &mut P,
    list: capnp::struct_list::Reader<'_, element::Owned>,
    scratch: &mut Path,
) -> Result<(), Error> {
    use element::Which;
    for node in list.iter() {
        match node.which()? {
            Which::Path(p) => {
                read_path_into(p?, scratch)?;
                paint.draw_path(scratch);
            }
            Which::Clipped(c) => {
                let c = c?;
                let clip = read_clip_path(c.get_clip()?)?;
                let children = c.get_elements()?;
                // with_clip returns the value of the closure, so the Result
                // of the nested walk comes straight out.
                paint.with_clip(&clip, |p2| stream_elements(p2, children, &mut *scratch))?;
            }
            Which::Text(t) => {
                let t = read_text_node(t?)?;
                paint.draw_text(&t);
            }
            Which::Bitmap(b) => {
                let b = read_bitmap(b?);
                paint.draw_bitmap(&b);
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_scene() -> Scene {
        let mut scene = Scene::new(120.0, 80.0);
        {
            let mut p = scene.path(PathStyle {
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
            let mut clip = scene.clip_rect(50.0, 50.0, 30.0, 20.0, 15.0, FillRule::EvenOdd);
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
            clip.bitmap(Bitmap {
                id: 7,
                // 64×64 asset, mirrored horizontally, rotated 90°, centred at (70, 40).
                transform: crate::scene::bitmap_box_affine(64, 64, 70.0, 40.0, -32.0, 32.0, 90.0),
            });
        }
        scene
    }

    fn assert_scene_eq(a: &Scene, b: &Scene) {
        assert_eq!(a.width, b.width);
        assert_eq!(a.height, b.height);
        assert_eq!(a.elements.len(), b.elements.len(), "node count");
        for (i, (x, y)) in a.elements.iter().zip(b.elements.iter()).enumerate() {
            assert_eq!(format!("{x:?}"), format!("{y:?}"), "node {i}");
        }
    }

    #[test]
    fn frame_round_trip_preserves_drawlist() {
        let scene = sample_scene();
        let bytes = encode_frame(&scene);
        match decode(&bytes).expect("decode") {
            Decoded::Frame(d) => assert_scene_eq(&scene, &d),
            other => panic!("expected Frame, got {other:?}"),
        }
    }

    #[test]
    fn empty_drawlist_round_trips() {
        let scene = Scene::new(640.0, 480.0);
        let bytes = encode_frame(&scene);
        match decode(&bytes).unwrap() {
            Decoded::Frame(d) => {
                assert_eq!(d.width, 640.0);
                assert_eq!(d.height, 480.0);
                assert!(d.elements.is_empty());
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
    fn a_path_with_coords_no_verb_claims_is_rejected() {
        // One Move claims 2 floats and 4 are present.
        let mut builder = MessageBuilder::new_default();
        {
            let msg = builder.init_root::<message::Builder>();
            let frame = msg.init_frame();
            let mut nodes = frame.init_elements(1);
            let node = nodes.reborrow().get(0);
            let mut p = node.init_path();
            let _ = p.reborrow().init_style();
            p.set_verbs(&[SegmentKind::Move as u8]);
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
    fn malformed_path_is_rejected() {
        // One Cubic claims 6 floats and 4 are present.
        let mut builder = MessageBuilder::new_default();
        {
            let msg = builder.init_root::<message::Builder>();
            let frame = msg.init_frame();
            let mut nodes = frame.init_elements(1);
            let node = nodes.reborrow().get(0);
            let mut p = node.init_path();
            let _ = p.reborrow().init_style();
            p.set_verbs(&[SegmentKind::Cubic as u8]);
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
        let mut scene = Scene::new(100.0, 50.0);
        {
            let mut p = scene.path(PathStyle {
                stroke: Paint::rgba(0, 0, 0, 1.0),
                stroke_width: 2.0,
                miter_limit: 7.5,
                dash: Dash::new(vec![4.0, 2.0, 1.0], 1.5).map(Box::new),
                ..PathStyle::default()
            });
            p.move_to(0.0, 0.0);
            p.line_to(50.0, 50.0);
        }
        let bytes = encode_frame(&scene);
        match decode(&bytes).unwrap() {
            Decoded::Frame(d) => {
                let node = d.elements.first().expect("one node");
                let Element::Path(p) = node else {
                    panic!("expected path");
                };
                assert_eq!(p.style.miter_limit, 7.5);
                let dash = p.style.dash.as_ref().unwrap();
                assert_eq!(dash.array(), [4.0, 2.0, 1.0]);
                assert_eq!(dash.offset(), 1.5);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn linear_gradient_paint_round_trips() {
        let mut scene = Scene::new(50.0, 50.0);
        let gradient = Gradient::linear(
            0.0,
            0.0,
            50.0,
            0.0,
            vec![
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
        );
        {
            let mut p = scene.path(PathStyle {
                fill: Paint::gradient(gradient.clone()),
                ..PathStyle::default()
            });
            p.move_to(0.0, 0.0);
            p.line_to(50.0, 0.0);
            p.line_to(50.0, 50.0);
        }
        let bytes = encode_frame(&scene);
        match decode(&bytes).unwrap() {
            Decoded::Frame(d) => {
                let Element::Path(p) = d.elements.first().unwrap() else {
                    panic!();
                };
                assert_eq!(p.style.fill, Paint::gradient(gradient));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn radial_gradient_paint_round_trips() {
        let mut scene = Scene::new(50.0, 50.0);
        let gradient = Gradient::radial(
            25.0,
            25.0,
            20.0,
            vec![
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
        );
        {
            let mut p = scene.path(PathStyle {
                fill: Paint::gradient(gradient.clone()),
                ..PathStyle::default()
            });
            p.move_to(0.0, 0.0);
            p.line_to(50.0, 50.0);
        }
        let bytes = encode_frame(&scene);
        match decode(&bytes).unwrap() {
            Decoded::Frame(d) => {
                let Element::Path(p) = d.elements.first().unwrap() else {
                    panic!();
                };
                assert_eq!(p.style.fill, Paint::gradient(gradient));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn gradient_spread_mode_round_trips() {
        let mut scene = Scene::new(50.0, 50.0);
        let linear = Gradient::linear(
            0.0,
            0.0,
            25.0,
            0.0,
            vec![
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
        )
        .with_spread(SpreadMode::Reflect);
        {
            let mut p = scene.path(PathStyle {
                fill: Paint::gradient(linear.clone()),
                ..PathStyle::default()
            });
            p.move_to(0.0, 0.0);
            p.line_to(50.0, 50.0);
        }
        let radial = Gradient::radial(
            25.0,
            25.0,
            10.0,
            vec![
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
        )
        .with_spread(SpreadMode::Repeat);
        {
            let mut p = scene.path(PathStyle {
                fill: Paint::gradient(radial.clone()),
                ..PathStyle::default()
            });
            p.move_to(0.0, 0.0);
            p.line_to(50.0, 50.0);
        }
        let bytes = encode_frame(&scene);
        match decode(&bytes).unwrap() {
            Decoded::Frame(d) => {
                let Element::Path(p0) = &d.elements[0] else {
                    panic!();
                };
                assert_eq!(p0.style.fill, Paint::gradient(linear));
                let Element::Path(p1) = &d.elements[1] else {
                    panic!();
                };
                assert_eq!(p1.style.fill, Paint::gradient(radial));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn clip_path_round_trips() {
        // Built without clip_rect, so the walk sees a quadratic and the
        // even-odd rule.
        let mut scene = Scene::new(50.0, 50.0);
        {
            let mut clip = scene.clip(
                ClipPath::builder(FillRule::EvenOdd)
                    .move_to(0.0, 0.0)
                    .line_to(30.0, 0.0)
                    .quad_to(40.0, 25.0, 30.0, 40.0)
                    .line_to(0.0, 40.0)
                    .build(),
            );
            let mut p = clip.path(PathStyle::default());
            p.move_to(0.0, 0.0);
            p.line_to(10.0, 10.0);
        }
        let bytes = encode_frame(&scene);
        match decode(&bytes).unwrap() {
            Decoded::Frame(d) => {
                let Element::Clipped { clip, elements } = &d.elements[0] else {
                    panic!("expected Clipped, got {:?}", d.elements[0]);
                };
                let segs: Vec<_> = clip.segments().collect();
                assert_eq!(
                    segs,
                    [
                        Segment::Move { x: 0.0, y: 0.0 },
                        Segment::Line { x: 30.0, y: 0.0 },
                        Segment::Quad {
                            cx: 40.0,
                            cy: 25.0,
                            x: 30.0,
                            y: 40.0
                        },
                        Segment::Line { x: 0.0, y: 40.0 },
                    ]
                );
                assert_eq!(clip.fill_rule, FillRule::EvenOdd);
                assert_eq!(elements.len(), 1);
                assert!(matches!(&elements[0], Element::Path(_)));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn malformed_clip_path_is_rejected() {
        // One Cubic claims 6 floats and 2 are present.
        let mut builder = MessageBuilder::new_default();
        {
            let msg = builder.init_root::<message::Builder>();
            let frame = msg.init_frame();
            let mut nodes = frame.init_elements(1);
            let node = nodes.reborrow().get(0);
            let clipped = node.init_clipped();
            let mut c = clipped.init_clip();
            c.set_verbs(&[SegmentKind::Cubic as u8]);
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
    fn nested_clips_round_trip() {
        let mut scene = Scene::new(100.0, 100.0);
        {
            let mut outer = scene.clip_rect(50.0, 50.0, 80.0, 80.0, 0.0, FillRule::NonZero);
            let mut p = outer.path(PathStyle::default());
            p.move_to(0.0, 0.0);
            p.line_to(100.0, 100.0);
            drop(p);
            let mut inner = outer.clip_rect(50.0, 50.0, 40.0, 40.0, 0.0, FillRule::EvenOdd);
            let mut p = inner.path(PathStyle::default());
            p.move_to(10.0, 10.0);
            p.line_to(20.0, 20.0);
        }
        let bytes = encode_frame(&scene);
        match decode(&bytes).unwrap() {
            Decoded::Frame(d) => {
                assert_eq!(d.elements.len(), 1);
                let Element::Clipped {
                    clip: _,
                    elements: outer_els,
                } = &d.elements[0]
                else {
                    panic!("expected outer Clipped");
                };
                assert_eq!(outer_els.len(), 2, "outer should hold path + inner clip");
                assert!(matches!(&outer_els[0], Element::Path(_)));
                let Element::Clipped {
                    clip: _,
                    elements: inner_els,
                } = &outer_els[1]
                else {
                    panic!("expected inner Clipped, got {:?}", outer_els[1]);
                };
                assert_eq!(inner_els.len(), 1);
                assert!(matches!(&inner_els[0], Element::Path(_)));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn default_style_uses_solid_transparent_paint() {
        let mut scene = Scene::new(10.0, 10.0);
        {
            let mut p = scene.path(PathStyle::default());
            p.move_to(0.0, 0.0);
        }
        let bytes = encode_frame(&scene);
        match decode(&bytes).unwrap() {
            Decoded::Frame(d) => {
                let Element::Path(p) = d.elements.first().unwrap() else {
                    panic!();
                };
                assert_eq!(p.style.fill, Paint::Solid(Rgba::default()));
                assert_eq!(p.style.stroke, Paint::Solid(Rgba::default()));
                assert!(p.style.dash.is_none());
                assert_eq!(p.style.miter_limit, crate::scene::DEFAULT_MITER_LIMIT);
            }
            _ => panic!(),
        }
    }
}

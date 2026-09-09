//! `Scene` and the values it holds, to and from the Cap'n Proto structs.
//!
//! Nothing here knows about the `Message` envelope. A caller hands in a
//! builder or a reader for the `Scene` struct of the schema, so the same
//! functions serve a frame inside a session and a scene on its own.

use crate::scene::{
    Bitmap, ClipPath, Dash, Element, FillRule, FontStyle, Gradient, GradientGeom, LineCap,
    LineJoin, Paint, Path, PathStyle, Rgba, Scene, Segment, SegmentKind, Segments, SpreadMode,
    Stop, TextNode,
};
use crate::scene_capnp::{
    FillRule as WFillRule, FontStyle as WFontStyle, LineCap as WLineCap, LineJoin as WLineJoin,
    SpreadMode as WSpreadMode, bitmap_node, clip_path as wire_clip_path, clipped as wire_clipped,
    element, paint as wire_paint, path as wire_path, path_style as wire_path_style,
    rgba as wire_rgba, scene as wire_scene, stop as wire_stop, text_node,
};

use super::Error;

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

pub(super) fn read_clip_path(r: wire_clip_path::Reader<'_>) -> Result<ClipPath, Error> {
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

pub(super) fn read_bitmap(r: bitmap_node::Reader<'_>) -> Bitmap {
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

pub(super) fn read_text_node(r: text_node::Reader<'_>) -> Result<TextNode, Error> {
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
pub(super) fn read_path_into(r: wire_path::Reader<'_>, path: &mut Path) -> Result<(), Error> {
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

pub(super) fn write_scene(mut b: wire_scene::Builder<'_>, scene: &Scene) {
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

pub(super) fn read_scene(r: wire_scene::Reader<'_>) -> Result<Scene, Error> {
    let mut out = Scene::new(r.get_width(), r.get_height());
    if r.has_elements() {
        out.elements = read_element_list(r.get_elements()?)?;
    }
    Ok(out)
}

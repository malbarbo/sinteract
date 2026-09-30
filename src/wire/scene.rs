//! `Scene` and the values it holds, to and from the Cap'n Proto structs.
//!
//! Nothing here knows about the `EngineToServer` envelope. A caller hands in
//! a builder or a reader for the `Scene` struct of the schema, so the same
//! functions serve a frame inside a session and a scene on its own.

use std::collections::BTreeSet;

use capnp::message::{Builder, HeapAllocator};

use crate::scene::{
    Bitmap, ClipPath, Dash, Element, FillRule, FontStyle, GradientGeometry, Image, LineCap,
    LineJoin, MAX_NESTING, Paint, Path, PathStyle, Rgba, Sampling, Scene, Segment, SegmentKind,
    Segments, SpreadMode, Stop, Stops, Text, TextSpec, end_segments, push_segment,
};
use crate::scene_capnp::{
    FillRule as WFillRule, FontStyle as WFontStyle, LineCap as WLineCap, LineJoin as WLineJoin,
    Sampling as WSampling, SpreadMode as WSpreadMode, bitmap, clip_path as wire_clip_path,
    clipped as wire_clipped, element, layer as wire_layer, paint as wire_paint, path as wire_path,
    path_style as wire_path_style, rgba as wire_rgba, scene as wire_scene, stop as wire_stop,
    text_node,
};

use super::{Error, ValueError, skip_unusable};

/// A message whose root is `scene`.
pub(super) fn scene_message(scene: &Scene, ids: &dyn Fn(&Image) -> u32) -> Builder<HeapAllocator> {
    let mut builder = Builder::new_default();
    write_scene(builder.init_root::<wire_scene::Builder>(), scene, ids);
    builder
}

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
        WLineCap::Butt => LineCap::Butt,
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
        WLineJoin::Miter => LineJoin::Miter,
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
        WFillRule::NonZero => FillRule::NonZero,
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
        WFontStyle::Normal => FontStyle::Normal,
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
        WSpreadMode::Pad => SpreadMode::Pad,
    }
}

// ---------------------------------------------------------------------------
// Struct writers (build into capnp builders)
// ---------------------------------------------------------------------------

fn write_rgba(mut b: wire_rgba::Builder<'_>, c: Rgba) {
    b.set_r(c.r);
    b.set_g(c.g);
    b.set_b(c.b);
    b.set_a(c.opacity());
}

fn read_rgba(r: wire_rgba::Reader<'_>) -> Rgba {
    Rgba {
        r: r.get_r(),
        g: r.get_g(),
        b: r.get_b(),
        // A NaN goes to 0, as `as u8` takes it.
        a: (r.get_a().clamp(0.0, 1.0) * 255.0).round() as u8,
    }
}

/// A `0xRRGGBBAA` color, as the fallback of a paint carries it.
fn rgba_from_u32(c: u32) -> Rgba {
    let [r, g, b, a] = c.to_be_bytes();
    Rgba { r, g, b, a }
}

fn write_stop(mut b: wire_stop::Builder<'_>, s: Stop) {
    b.set_offset(s.offset);
    write_rgba(b.reborrow().init_color(), s.color);
}

fn read_stop(r: wire_stop::Reader<'_>) -> Result<Stop, ValueError> {
    Ok(Stop {
        offset: r.get_offset(),
        color: read_rgba(r.get_color()?),
    })
}

fn write_stops(mut b: capnp::struct_list::Builder<'_, wire_stop::Owned>, stops: &Stops) {
    for (i, s) in stops.iter().enumerate() {
        write_stop(b.reborrow().get(i as u32), *s);
    }
}

fn read_stops(
    r: capnp::struct_list::Reader<'_, wire_stop::Owned>,
) -> Result<Vec<Stop>, ValueError> {
    r.iter().map(read_stop).collect()
}

fn write_paint(b: wire_paint::Builder<'_>, p: &Paint) {
    match p {
        Paint::Solid(c) => write_rgba(b.init_solid(), *c),
        Paint::Gradient(g) => match g.geometry() {
            GradientGeometry::Linear { x0, y0, x1, y1 } => {
                let mut b = b.init_linear();
                b.set_x0(x0);
                b.set_y0(y0);
                b.set_x1(x1);
                b.set_y1(y1);
                b.set_spread(spread_to_wire(g.spread()));
                write_stops(b.init_stops(g.stops().len() as u32), g.stops());
            }
            GradientGeometry::Radial { cx, cy, radius } => {
                let mut b = b.init_radial();
                b.set_cx(cx);
                b.set_cy(cy);
                b.set_radius(radius);
                b.set_spread(spread_to_wire(g.spread()));
                write_stops(b.init_stops(g.stops().len() as u32), g.stops());
            }
        },
    }
}

fn read_paint(r: wire_paint::Reader<'_>) -> Result<Paint, ValueError> {
    use wire_paint::Which;
    let which = match r.which() {
        Ok(which) => which,
        // An arm from a newer schema draws the fallback color of its writer.
        // Without one, the element that holds the paint is skipped.
        Err(_) if r.get_has_fallback() => {
            return Ok(Paint::Solid(rgba_from_u32(r.get_fallback())));
        }
        Err(e) => return Err(e.into()),
    };
    // A null stops pointer reads as an empty list, so an absent ramp needs
    // no guard.
    let (geometry, stops, spread) = match which {
        Which::Solid(c) => return Ok(Paint::Solid(read_rgba(c?))),
        Which::Linear(g) => {
            let g = g?;
            let geometry = GradientGeometry::Linear {
                x0: g.get_x0(),
                y0: g.get_y0(),
                x1: g.get_x1(),
                y1: g.get_y1(),
            };
            (geometry, read_stops(g.get_stops()?)?, g.get_spread()?)
        }
        Which::Radial(g) => {
            let g = g?;
            let geometry = GradientGeometry::Radial {
                cx: g.get_cx(),
                cy: g.get_cy(),
                radius: g.get_radius(),
            };
            (geometry, read_stops(g.get_stops()?)?, g.get_spread()?)
        }
    };
    Ok(Paint::gradient(geometry, stops, spread_from_wire(spread)))
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

fn read_path_style(r: wire_path_style::Reader<'_>) -> Result<PathStyle, ValueError> {
    // Dash::new returns None for an array that draws a solid stroke, such as
    // an empty one, and drops any stray offset.
    let dash = Dash::new(
        r.get_dash_array()?.iter().collect::<Vec<_>>(),
        r.get_dash_offset(),
    );
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

fn read_clip_path(r: wire_clip_path::Reader<'_>) -> Result<ClipPath, ValueError> {
    let mut clip = ClipPath::default();
    clip.fill_rule = fill_rule_from_wire(r.get_fill_rule()?);
    read_segments(clip.segments_mut(), r.get_verbs()?, r.get_coords()?)?;
    Ok(clip)
}

fn write_bitmap(mut b: bitmap::Builder<'_>, n: &Bitmap, ids: &dyn Fn(&Image) -> u32) {
    b.set_id(ids(&n.image));
    b.set_m0(n.transform[0]);
    b.set_m1(n.transform[1]);
    b.set_m2(n.transform[2]);
    b.set_m3(n.transform[3]);
    b.set_m4(n.transform[4]);
    b.set_m5(n.transform[5]);
    b.set_sampling(match n.sampling {
        Sampling::Smooth => WSampling::Smooth,
        Sampling::Nearest => WSampling::Nearest,
    });
}

fn read_bitmap(
    r: bitmap::Reader<'_>,
    images: &dyn Fn(u32) -> Option<Image>,
) -> Result<Bitmap, ValueError> {
    let bitmap = Bitmap {
        image: images(r.get_id()).ok_or(ValueError::NoImage)?,
        transform: [
            r.get_m0(),
            r.get_m1(),
            r.get_m2(),
            r.get_m3(),
            r.get_m4(),
            r.get_m5(),
        ],
        // A sampling from a newer schema is a hint, so it draws smooth.
        sampling: match r.get_sampling() {
            Ok(WSampling::Nearest) => Sampling::Nearest,
            Ok(WSampling::Smooth) | Err(_) => Sampling::Smooth,
        },
    };
    Ok(bitmap)
}

fn write_text_node(mut b: text_node::Builder<'_>, n: &Text) {
    write_paint(b.reborrow().init_fill(), &n.fill);
    write_paint(b.reborrow().init_stroke(), &n.stroke);
    b.set_stroke_width(n.stroke_width);
    b.set_m0(n.transform[0]);
    b.set_m1(n.transform[1]);
    b.set_m2(n.transform[2]);
    b.set_m3(n.transform[3]);
    b.set_m4(n.transform[4]);
    b.set_m5(n.transform[5]);
    b.set_size(n.spec.size);
    b.set_family(&*n.spec.family);
    b.set_weight(n.spec.weight);
    b.set_style(font_style_to_wire(n.spec.style));
    b.set_underline(n.underline);
    b.set_text(&*n.spec.text);
}

fn read_text_node(r: text_node::Reader<'_>) -> Result<Text, ValueError> {
    let text = Text {
        fill: read_paint(r.get_fill()?)?,
        stroke: read_paint(r.get_stroke()?)?,
        stroke_width: r.get_stroke_width(),
        transform: [
            r.get_m0(),
            r.get_m1(),
            r.get_m2(),
            r.get_m3(),
            r.get_m4(),
            r.get_m5(),
        ],
        spec: TextSpec {
            size: r.get_size(),
            family: r.get_family()?.to_str()?.into(),
            weight: r.get_weight(),
            style: font_style_from_wire(r.get_style()?),
            text: r.get_text()?.to_str()?.to_owned(),
        },
        underline: r.get_underline(),
    };
    Ok(text)
}

/// [`ValueError::NotFinite`] unless `is_finite`, the rule of [`Scene`] for
/// an element, which the events follow too.
pub(super) fn check_finite(is_finite: bool) -> Result<(), ValueError> {
    if is_finite {
        Ok(())
    } else {
        Err(ValueError::NotFinite)
    }
}

// ---------------------------------------------------------------------------
// Scene <-> wire
// ---------------------------------------------------------------------------

// A `Path` holds typed segments. The flat verb and coord pair exists only on
// the wire, and the functions below are the only place where the two forms
// meet.

/// Number of floats that `segs` take on the wire.
fn coord_count(segs: Segments<'_>) -> u32 {
    segs.map(|s| s.kind().coord_count() as u32).sum()
}

/// A `SegmentKind` discriminant is the wire byte, so a verb is a cast. The
/// coords go in a second pass because a capnp builder hands out one field at
/// a time.
fn write_verbs(out: capnp::data::Builder<'_>, segs: Segments<'_>) {
    for (dst, seg) in out.iter_mut().zip(segs) {
        *dst = seg.kind() as u8;
    }
}

/// Write the coordinates in verb order, the layout that
/// [`read_segments`] reads.
fn write_coords(out: &mut capnp::primitive_list::Builder<'_, f32>, segs: Segments<'_>) {
    let mut i = 0;
    for seg in segs {
        seg.with_coords(|coords| {
            for &c in coords {
                out.set(i, c);
                i += 1;
            }
        });
    }
}

/// Rebuild the segments into `out`, reusing its allocation. A path whose
/// first verb is not a move begins at `(0, 0)`, and a move that no segment
/// follows is dropped, as the builders do. An unknown verb byte is a
/// value from a newer schema, and a coord stream that does not hold exactly
/// what the verbs claim is damage.
fn read_segments(
    out: &mut Vec<Segment>,
    verbs: &[u8],
    coords: capnp::primitive_list::Reader<'_, f32>,
) -> Result<(), ValueError> {
    let mismatch = || {
        ValueError::Malformed(Error::PathLengthMismatch {
            verbs: verbs.len(),
            coords: coords.len() as usize,
        })
    };
    out.clear();
    out.reserve(verbs.len() + 1);
    // A move of the wire replaces this one.
    out.push(Segment::Move { x: 0.0, y: 0.0 });
    let mut i = 0;
    for &b in verbs {
        let kind = SegmentKind::from_u8(b).ok_or(ValueError::Newer)?;
        if i + kind.coord_count() as u32 > coords.len() {
            return Err(mismatch());
        }
        let c = |k: u32| coords.get(i + k);
        let seg = match kind {
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
        };
        push_segment(out, seg);
        i += kind.coord_count() as u32;
    }
    // Coords that no verb claims are a mismatch too.
    if i != coords.len() {
        return Err(mismatch());
    }
    end_segments(out);
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

fn read_path(r: wire_path::Reader<'_>) -> Result<Path, ValueError> {
    let mut path = Path::default();
    path.style = read_path_style(r.get_style()?)?;
    read_segments(path.segments_mut(), r.get_verbs()?, r.get_coords()?)?;
    Ok(path)
}

fn write_element(b: element::Builder<'_>, node: &Element, ids: &dyn Fn(&Image) -> u32) {
    match node {
        Element::Path(p) => write_path(b.init_path(), p),
        Element::Clipped { clip, elements } => write_clipped(b.init_clipped(), clip, elements, ids),
        Element::Text(t) => write_text_node(b.init_text(), t),
        Element::Bitmap(n) => write_bitmap(b.init_bitmap(), n, ids),
        Element::Layer { opacity, elements } => {
            write_layer(b.init_layer(), *opacity, elements, ids)
        }
    }
}

fn write_layer(
    mut b: wire_layer::Builder<'_>,
    opacity: f32,
    elements: &[Element],
    ids: &dyn Fn(&Image) -> u32,
) {
    b.set_opacity(opacity);
    write_element_list(b.init_elements(elements.len() as u32), elements, ids);
}

fn write_clipped(
    mut b: wire_clipped::Builder<'_>,
    clip: &ClipPath,
    elements: &[Element],
    ids: &dyn Fn(&Image) -> u32,
) {
    write_clip_path(b.reborrow().init_clip(), clip);
    write_element_list(b.init_elements(elements.len() as u32), elements, ids);
}

fn write_element_list(
    mut list: capnp::struct_list::Builder<'_, element::Owned>,
    elements: &[Element],
    ids: &dyn Fn(&Image) -> u32,
) {
    for (i, node) in elements.iter().enumerate() {
        write_element(list.reborrow().get(i as u32), node, ids);
    }
}

/// Write `scene`, with the id that `ids` gives the image of each bitmap.
fn write_scene(mut b: wire_scene::Builder<'_>, scene: &Scene, ids: &dyn Fn(&Image) -> u32) {
    b.set_width(scene.width());
    b.set_height(scene.height());
    write_element_list(
        b.init_elements(scene.elements().len() as u32),
        scene.elements(),
        ids,
    );
}

/// Add the element in `node` to `scene` through its builder, which drops
/// what the scene drops. It skips an element of an arm from a newer schema,
/// one that holds a value from a newer schema, and a bitmap of an id that
/// `images` has no image for. A clip or a layer past [`MAX_NESTING`] is
/// skipped before its elements are read, since Cap'n Proto refuses a
/// message that nests far deeper.
fn read_element(
    node: element::Reader<'_>,
    scene: &mut Scene,
    images: &dyn Fn(u32) -> Option<Image>,
) -> Result<(), Error> {
    let Ok(which) = node.which() else {
        return Ok(());
    };
    skip_unusable(read_known_element(which, scene, images))?;
    Ok(())
}

fn read_known_element(
    which: element::WhichReader<'_>,
    scene: &mut Scene,
    images: &dyn Fn(u32) -> Option<Image>,
) -> Result<(), ValueError> {
    use element::Which;
    if matches!(which, Which::Clipped(_) | Which::Layer(_)) && scene.is_nested_to_max() {
        return Ok(());
    }
    match which {
        Which::Path(p) => scene.add_path(read_path(p?)?),
        Which::Clipped(c) => {
            let c = c?;
            let clip = read_clip_path(c.get_clip()?)?;
            let elements = c.get_elements()?;
            scene.clip(clip, |inner| read_element_list(elements, inner, images))?;
        }
        Which::Text(t) => scene.add_text(read_text_node(t?)?),
        Which::Bitmap(n) => scene.add_bitmap(read_bitmap(n?, images)?),
        Which::Layer(l) => {
            let l = l?;
            let elements = l.get_elements()?;
            scene.layer(l.get_opacity(), |inner| {
                read_element_list(elements, inner, images)
            })?;
        }
    }
    Ok(())
}

fn read_element_list(
    list: capnp::struct_list::Reader<'_, element::Owned>,
    scene: &mut Scene,
    images: &dyn Fn(u32) -> Option<Image>,
) -> Result<(), Error> {
    for node in list {
        read_element(node, scene, images)?;
    }
    Ok(())
}

/// Add to `ids` the id of each bitmap of the scene in `r`, and in what a
/// clip or a layer holds, with no decode of the rest. An element of an arm
/// from a newer schema holds no bitmap, and neither does a clip or a layer
/// past [`MAX_NESTING`], which a reader skips. The walk skips nothing else,
/// so `ids` may hold the id of a bitmap that a reader drops, such as one in
/// a hidden layer.
pub(super) fn read_bitmap_ids(
    r: wire_scene::Reader<'_>,
    ids: &mut BTreeSet<u32>,
) -> Result<(), Error> {
    if r.has_elements() {
        add_bitmap_ids(r.get_elements()?, ids, 0)?;
    }
    Ok(())
}

fn add_bitmap_ids(
    list: capnp::struct_list::Reader<'_, element::Owned>,
    ids: &mut BTreeSet<u32>,
    depth: usize,
) -> Result<(), Error> {
    for node in list {
        match node.which() {
            Ok(element::Which::Bitmap(b)) => {
                ids.insert(b?.get_id());
            }
            Ok(element::Which::Clipped(_) | element::Which::Layer(_)) if depth >= MAX_NESTING => {}
            Ok(element::Which::Clipped(c)) => add_bitmap_ids(c?.get_elements()?, ids, depth + 1)?,
            Ok(element::Which::Layer(l)) => add_bitmap_ids(l?.get_elements()?, ids, depth + 1)?,
            Ok(element::Which::Path(_) | element::Which::Text(_)) | Err(_) => {}
        }
    }
    Ok(())
}

/// Read the scene in `r`, with the image that `images` gives the id of each
/// bitmap.
pub(super) fn read_scene(
    r: wire_scene::Reader<'_>,
    images: &dyn Fn(u32) -> Option<Image>,
) -> Result<Scene, Error> {
    let mut scene = Scene::new(r.get_width(), r.get_height());
    if r.has_elements() {
        read_element_list(r.get_elements()?, &mut scene, images)?;
    }
    Ok(scene)
}

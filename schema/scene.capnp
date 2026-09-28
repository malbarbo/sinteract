@0xe6fe272699b1f2b7;

# The drawing vocabulary of sinteract. A `Scene` is the draw list that a
# renderer replays. Nothing here knows that a session exists, so an engine
# that paints locally reads this file alone.
#
# This file is the source of truth for the drawing format. Evolve it by
# appending fields with defaults. Never reorder or renumber, and let a union
# only grow.
#
# A reader skips an element whose arm it does not know, and an element that
# holds a value it does not know, such as a paint arm, an enum value or a
# verb byte, and draws the rest. A clip that holds one is skipped with all it
# holds. A hint, such as a sampling, is the exception, and a value it does
# not know takes the default. A paint of an arm it does not know draws its fallback color, when
# the writer set one, instead. Damage, such as a bad pointer or verbs that
# disagree with their coords, makes the whole scene unusable.
#
# A float that is not finite draws nothing. A reader skips an element that
# holds one in any float, drawn or not, such as the width of a transparent
# stroke, and a clip that holds one is skipped with all it holds. The dash
# is the exception: its comment says when it makes the stroke solid.
#
# Regenerate the Rust bindings for the three schema files with:
#   capnp compile -orust:src/wire --src-prefix=schema \
#     schema/scene.capnp schema/event.capnp schema/protocol.capnp
#   cargo fmt
#
# The generated files are committed at src/wire/*_capnp.rs, so the build
# does not need the capnp CLI. Do not edit them by hand.

struct Rgba {
    r @0 :UInt8;
    g @1 :UInt8;
    b @2 :UInt8;
    a @3 :Float32;
}

enum LineCap {
    butt   @0;
    round  @1;
    square @2;
}

enum LineJoin {
    miter  @0;
    round  @1;
    bevel  @2;
}

enum FillRule {
    nonZero @0;
    evenOdd @1;
}

enum FontStyle {
    normal  @0;
    italic  @1;
    oblique @2;
}

# ----- Paint -----
#
# A fill or a stroke is a solid color or a gradient. The stops rise by
# offset, in [0, 1]. A reader raises a stop that is below the one before it
# and clamps every offset to that range, as SVG and Skia do. A renderer
# without gradients uses the color of the first stop.
#
# A gradient with no extent paints the color of its last stop, as in SVG. A
# radius, or a distance between the ends of a line, of 2^-15 or less is no
# extent, because below that a rasterizer cannot tell it from zero.
#
# At the other end, a line longer than a 32 bit float measures as infinite,
# so a reader scales such a line about the origin until it measures. The
# color under a path moves by less than a step of that float.

struct Stop {
    offset @0 :Float32;
    color  @1 :Rgba;
}

enum SpreadMode {
    pad     @0;
    reflect @1;
    repeat  @2;
}

struct LinearGradient {
    x0     @0 :Float32;
    y0     @1 :Float32;
    x1     @2 :Float32;
    y1     @3 :Float32;
    stops  @4 :List(Stop);
    spread @5 :SpreadMode;
}

struct RadialGradient {
    cx     @0 :Float32;
    cy     @1 :Float32;
    radius @2 :Float32;
    stops  @3 :List(Stop);
    spread @4 :SpreadMode;
}

struct Paint {
    union {
        solid  @0 :Rgba;
        linear @1 :LinearGradient;
        radial @2 :RadialGradient;
    }

    # The color, as 0xRRGGBBAA, that a reader draws when it does not know the
    # arm above. A writer of an arm newer than radial sets it and
    # hasFallback. Without it, a reader skips the element. Both fit in the
    # word of the union tag, so an unset fallback costs no bytes.
    fallback    @3 :UInt32;
    hasFallback @4 :Bool;
}

# ----- Path / clip / text / bitmap nodes -----

struct PathStyle {
    fill        @0 :Paint;
    stroke      @1 :Paint;
    strokeWidth @2 :Float32;
    lineCap     @3 :LineCap;
    lineJoin    @4 :LineJoin;
    fillRule    @5 :FillRule;
    # Closes every sub-path, so the stroke joins at the start of each one.
    closed      @6 :Bool;
    # A miter limit below 1 is 1. The limit compares a ratio that is never
    # below 1, so the two say the same thing, and SVG rejects a smaller one.
    miterLimit  @7 :Float32;
    # Alternating on and off lengths. An odd list repeats to an even one.
    # An empty list, a negative or non-finite length, lengths that sum to
    # zero or a non-finite offset make the stroke solid.
    dashArray   @8 :List(Float32);
    dashOffset  @9 :Float32;
}

# A clip region, with the encoding of Path. A sub-path closes implicitly, as
# in an SVG clipPath, so the caller does not add the line back to the start.
# A clip with no area, such as one with no verbs, hides everything it holds.
struct ClipPath {
    verbs    @0 :Data;
    coords   @1 :List(Float32);
    fillRule @2 :FillRule;
}

# The image fills the unit square (-0.5..0.5, -0.5..0.5) whatever its size,
# and the affine maps that square onto the canvas, in the convention of
# TextNode. The producer puts the size, the rotation and the mirroring into
# it (see sinteract::scene::Bitmap::fit).
struct Bitmap {
    id       @0 :UInt32;
    m0       @1 :Float32;
    m1       @2 :Float32;
    m2       @3 :Float32;
    m3       @4 :Float32;
    m4       @5 :Float32;
    m5       @6 :Float32;
    # A hint, so a reader draws a sampling it does not know as smooth.
    sampling @7 :Sampling;
}

enum Sampling {
    smooth  @0;
    nearest @1;
}

# A glyph is drawn in text space, with `size` units to the em and the origin
# at the center of a box that spans the advance of the text and the ascender
# to the descender of the face. The affine maps it to the canvas, in the
# convention of the PDF cm operator and of SVG matrix(...):
#   x' = m0 * x + m2 * y + m4
#   y' = m1 * x + m3 * y + m5
# The producer puts the fit, the rotation and the mirroring into it (see
# sinteract::scene::TextSpec::fit).
#
# `text` draws on one line. A tab advances by the width of eight spaces of
# the face. Any other control character draws nothing, so a newline does not
# break the line.
#
# The name keeps the Node suffix. A struct named Text hides the built-in
# Text type from every field of this file.
struct TextNode {
    fill        @0  :Rgba;
    stroke      @1  :Rgba;
    strokeWidth @2  :Float32;
    m0          @3  :Float32;
    m1          @4  :Float32;
    m2          @5  :Float32;
    m3          @6  :Float32;
    m4          @7  :Float32;
    m5          @8  :Float32;
    size        @9  :Float32;
    family      @10 :Text;
    weight      @11 :UInt16;
    style       @12 :FontStyle;
    underline   @13 :Bool;
    text        @14 :Text;
}

# ----- Path -----
#
# `verbs` has one byte per segment, and `coords` holds the floats the verbs
# consume, in order:
#   0 = move    (2 floats: x, y)
#   1 = line    (2 floats: x, y)
#   2 = quad    (4 floats: cx, cy, x, y)
#   3 = cubic   (6 floats: c1x, c1y, c2x, c2y, x, y)
# A path whose first verb is not a move begins at (0, 0), as if a move to
# (0, 0) came first. A move that no other verb follows draws nothing, and a
# decoder drops it. A decoder rejects a path whose coords do not match its
# verbs.

struct Path {
    style  @0 :PathStyle;
    verbs  @1 :Data;
    coords @2 :List(Float32);
}

# ----- Scene element -----
#
# A clipped element nests its own elements, so the clip nesting is balanced
# by construction. A renderer may draw nothing for an element that reaches
# more than 2^28 output pixels from the origin.

struct Element {
    union {
        path    @0 :Path;
        clipped @1 :Clipped;
        text    @2 :TextNode;
        bitmap  @3 :Bitmap;
    }
}

struct Clipped {
    clip     @0 :ClipPath;
    elements @1 :List(Element);
}

# A width or a height that describes no frame, which is one that is not
# finite or is negative, is 0. A frame of no width or no height is empty.
struct Scene {
    width    @0 :Float32;
    height   @1 :Float32;
    elements @2 :List(Element);
}

# The verb bytes, for a reader that is not Rust. Rust uses
# sinteract::scene::SegmentKind.
const verbMove  :UInt8 = 0;
const verbLine  :UInt8 = 1;
const verbQuad  :UInt8 = 2;
const verbCubic :UInt8 = 3;

@0x9e6ad945a7c8b88a;

# sinteract wire format.
#
# Three messages travel between the server, which runs the engine, and the
# client, which renders. Asset uploads a bitmap once, at the start of the
# session, and a frame references it by id. Frame is the scene to paint.
# Event is the input from the client. Close ends the session.
#
# The schema is the source of truth. Evolve it by appending fields with
# defaults. Never reorder or renumber. Every host reads the same generated
# bindings.
#
# Regenerate the Rust bindings with:
#   capnp compile -orust --src-prefix=schema -o src/wire schema/frame.capnp
#
# When capnp cannot spawn the capnpc-rust plugin, run the two steps by hand:
#   capnp compile -o- --src-prefix=schema schema/frame.capnp \
#     | capnpc-rust && mv frame_capnp.rs src/wire/
#
# The generated file is committed at src/wire/frame_capnp.rs, so the build
# does not need the capnp CLI.

# ----- Common scalar types -----

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

enum KeyKind {
    press @0;
    down  @1;
    up    @2;
}

# ----- Paint -----
#
# A fill or a stroke is a solid color or a gradient. The stops are sorted by
# offset, in [0, 1]. A renderer without gradients uses the color of the first
# stop.

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
}

# ----- Path / clip / text / bitmap nodes -----

struct PathStyle {
    fill        @0 :Paint;
    stroke      @1 :Paint;
    strokeWidth @2 :Float32;
    lineCap     @3 :LineCap;
    lineJoin    @4 :LineJoin;
    fillRule    @5 :FillRule;
    closed      @6 :Bool;
    miterLimit  @7 :Float32;
    dashArray   @8 :List(Float32);
    dashOffset  @9 :Float32;
}

# A clip region, with the encoding of Path. A sub-path closes implicitly, as
# in an SVG clipPath, so the caller does not add the line back to the start.
struct ClipPath {
    verbs    @0 :Data;
    coords   @1 :List(Float32);
    fillRule @2 :FillRule;
}

# The affine maps the image pixels (0..img_w, 0..img_h) onto the canvas, in
# the convention of TextNode. The producer puts the fit, the rotation and the
# mirroring into it (see sinteract::scene::bitmap_box_affine).
struct BitmapNode {
    id @0 :UInt32;
    m0 @1 :Float32;
    m1 @2 :Float32;
    m2 @3 :Float32;
    m3 @4 :Float32;
    m4 @5 :Float32;
    m5 @6 :Float32;
}

# A glyph is drawn in text space, with the origin at the left of the baseline
# and `size` pixels per em, and the affine maps it to the canvas, in the
# convention of the PDF cm operator and of SVG matrix(...):
#   x' = m0 * x + m2 * y + m4
#   y' = m1 * x + m3 * y + m5
# The producer puts the fit, the rotation and the mirroring into it (see
# sinteract::scene::text_box_affine).
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
# A decoder rejects a path whose coords do not match its verbs.

struct Path {
    style  @0 :PathStyle;
    verbs  @1 :Data;
    coords @2 :List(Float32);
}

# ----- Scene element -----
#
# A clipped element nests its own elements, so the clip nesting is balanced
# by construction.

struct Element {
    union {
        path    @0 :Path;
        clipped @1 :Clipped;
        text    @2 :TextNode;
        bitmap  @3 :BitmapNode;
    }
}

struct Clipped {
    clip     @0 :ClipPath;
    elements @1 :List(Element);
}

struct Scene {
    width    @0 :Float32;
    height   @1 :Float32;
    elements @2 :List(Element);
}

# ----- Input events (client → server) -----

struct KeyEvent {
    kind      @0 :KeyKind;
    key       @1 :Text;
    modifiers @2 :UInt8;
}

struct InputEvent {
    union {
        key   @0 :KeyEvent;
        tick  @1 :Void;
        close @2 :Void;
    }
}

# ----- Top-level message envelope -----
#
# Server to client:
#   * asset        (one per bitmap, before the frames)
#   * frame        (one per repaint)
#   * sessionClose
#
# Client to server:
#   * event
#   * sessionClose

struct AssetMsg {
    id   @0 :UInt32;
    blob @1 :Data;
    # MIME hint such as "image/png". A renderer sniffs the blob when it is
    # empty.
    mime @2 :Text;
}

struct Message {
    union {
        asset        @0 :AssetMsg;
        frame        @1 :Scene;
        event        @2 :InputEvent;
        sessionClose @3 :Void;
    }
}

# The verb bytes, for a host that is not Rust. Rust uses
# sinteract::scene::SegmentKind.
const verbMove  :UInt8 = 0;
const verbLine  :UInt8 = 1;
const verbQuad  :UInt8 = 2;
const verbCubic :UInt8 = 3;

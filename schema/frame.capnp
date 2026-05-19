@0x9e6ad945a7c8b88a;

# simage wire format — Cap'n Proto schema.
#
# Three messages travel between server (engine host) and client (renderer):
#   * Asset    — uploaded once at session start; referenced by id afterwards
#   * Frame    — the typed draw list to paint this frame (server → client)
#   * Event    — input arriving from a client (client → server)
# plus a Close.
#
# The schema is the source of truth for evolution: append fields with
# defaults, never reorder. Every host (Rust, JS, future) reads the same
# generated bindings.
#
# Regenerate the Rust bindings with:
#   capnp compile -orust --src-prefix=schema -o src/wire schema/frame.capnp
#
# (Equivalent two-step form, useful when the sandbox blocks `capnp` from
# spawning the `capnpc-rust` plugin directly:
#   capnp compile -o- --src-prefix=schema schema/frame.capnp \
#     | capnpc-rust && mv frame_capnp.rs src/wire/)
#
# The generated file is committed at src/wire/frame_capnp.rs (no `capnp` CLI
# dependency at build time).

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

# ----- Paint — fill/stroke can be solid or a gradient. -----
#
# `stops` is a list of (offset, color) pairs with offset in [0, 1], sorted
# ascending. Renderers that do not understand gradients fall back to the
# first stop's color as a solid.

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

# Arbitrary clip region. `verbs` / `coords` follow the same encoding as
# `Path` (see below); `fillRule` decides which sub-regions count as inside.
# Sub-paths are treated as implicitly closed (SVG `<clipPath>` semantics),
# so callers do not have to add a final line back to the starting point.
struct ClipPath {
    verbs    @0 :Data;
    coords   @1 :List(Float32);
    fillRule @2 :FillRule;
}

# The 6-float affine maps the bitmap's natural image-pixel coordinates
# `(0..img_w, 0..img_h)` onto the canvas — same `cm` / `matrix(...)`
# convention as TextNode. The producer is expected to bake fit-to-box,
# rotation, and mirroring into this matrix (see `simage::ir::bitmap_box_affine`).
struct BitmapNode {
    id @0 :UInt32;
    m0 @1 :Float32;
    m1 @2 :Float32;
    m2 @3 :Float32;
    m3 @4 :Float32;
    m4 @5 :Float32;
    m5 @6 :Float32;
}

# Glyphs are drawn in "natural" text space (origin at baseline-left, units in
# `size`-pixel font units) and then mapped to canvas pixels by the affine
# below. Convention follows PDF's `cm` operator and SVG `matrix(...)`:
#   x' = m0 * x + m2 * y + m4
#   y' = m1 * x + m3 * y + m5
# The producer is expected to bake fit-to-box, rotation, and mirroring into
# this matrix (see `simage::ir::text_box_affine` for the canonical helper).
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

# ----- Path — verbs+coords, SVG-style. -----
#
# `verbs` is one byte per segment:
#   0 = move    (consumes 2 floats: x, y)
#   1 = line    (consumes 2 floats: x, y)
#   2 = quad    (consumes 4 floats: cx, cy, x, y)
#   3 = cubic   (consumes 6 floats: c1x, c1y, c2x, c2y, x, y)
# `coords` is the flat float stream consumed by the verbs in order. Decoders
# reject paths whose verbs and coords lengths disagree.

struct Path {
    style  @0 :PathStyle;
    verbs  @1 :Data;
    coords @2 :List(Float32);
}

# ----- Scene element — union is native in Cap'n Proto, no wrapper struct. -----

struct Element {
    union {
        path     @0 :Path;
        clipPush @1 :ClipPath;
        clipPop  @2 :Void;
        text     @3 :TextNode;
        bitmap   @4 :BitmapNode;
    }
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
# Server → client:
#   * asset        (one per bitmap, before frames)
#   * frame        (one per repaint)
#   * sessionClose
#
# Client → server:
#   * event
#   * sessionClose

struct AssetMsg {
    id   @0 :UInt32;
    blob @1 :Data;
    # MIME hint, e.g. "image/png". Optional; renderer can sniff if empty.
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

# Verb constants (mirrored in simage::ir::verb on the Rust side).
const verbMove  :UInt8 = 0;
const verbLine  :UInt8 = 1;
const verbQuad  :UInt8 = 2;
const verbCubic :UInt8 = 3;

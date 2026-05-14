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

# ----- Path / clip / text / bitmap nodes -----

struct PathStyle {
    fill        @0 :Rgba;
    stroke      @1 :Rgba;
    strokeWidth @2 :Float32;
    lineCap     @3 :LineCap;
    lineJoin    @4 :LineJoin;
    fillRule    @5 :FillRule;
    closed      @6 :Bool;
}

struct ClipBox {
    cx    @0 :Float32;
    cy    @1 :Float32;
    w     @2 :Float32;
    h     @3 :Float32;
    angle @4 :Float32;
}

struct BitmapNode {
    id     @0 :UInt32;
    cx     @1 :Float32;
    cy     @2 :Float32;
    w      @3 :Float32;
    h      @4 :Float32;
    angle  @5 :Float32;
    flipH  @6 :Bool;
    flipV  @7 :Bool;
}

struct TextNode {
    fill        @0  :Rgba;
    stroke      @1  :Rgba;
    strokeWidth @2  :Float32;
    lineCap     @3  :LineCap;
    lineJoin    @4  :LineJoin;
    cx          @5  :Float32;
    cy          @6  :Float32;
    bw          @7  :Float32;
    bh          @8  :Float32;
    angle       @9  :Float32;
    flipH       @10 :Bool;
    flipV       @11 :Bool;
    size        @12 :Float32;
    family      @13 :Text;
    weight      @14 :UInt16;
    style       @15 :FontStyle;
    underline   @16 :Bool;
    text        @17 :Text;
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

# ----- Draw node — union is native in Cap'n Proto, no wrapper struct. -----

struct DrawNode {
    union {
        path     @0 :Path;
        clipPush @1 :ClipBox;
        clipPop  @2 :Void;
        text     @3 :TextNode;
        bitmap   @4 :BitmapNode;
    }
}

struct DrawList {
    width  @0 :Float32;
    height @1 :Float32;
    nodes  @2 :List(DrawNode);
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
        frame        @1 :DrawList;
        event        @2 :InputEvent;
        sessionClose @3 :Void;
    }
}

# Verb constants (mirrored in simage::ir::verb on the Rust side).
const verbMove  :UInt8 = 0;
const verbLine  :UInt8 = 1;
const verbQuad  :UInt8 = 2;
const verbCubic :UInt8 = 3;

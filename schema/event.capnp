@0x910b52243f5e98f9;

# The input vocabulary of sinteract. Every frontend turns terminal, window
# and browser input into these, and they cross the wire unchanged.
#
# This file is the source of truth for the input format. The same rules as
# scene.capnp apply. See its header for how to regenerate the bindings.

enum KeyKind {
    press @0;
    down  @1;
    up    @2;
}

# ----- Input events (client → server) -----

struct KeyEvent {
    kind   @0 :KeyKind;
    # The W3C KeyboardEvent.key value, such as "ArrowLeft", "a" or " ".
    key    @1 :Text;
    alt    @2 :Bool;
    ctrl   @3 :Bool;
    shift  @4 :Bool;
    # The Windows, Command or Super key.
    meta   @5 :Bool;
    # The key is held and the system repeats it.
    repeat @6 :Bool;
}

struct InputEvent {
    union {
        key   @0 :KeyEvent;
        tick  @1 :Void;
        close @2 :Void;
    }
}

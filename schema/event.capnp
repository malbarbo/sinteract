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

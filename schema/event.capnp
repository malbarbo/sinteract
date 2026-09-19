@0x910b52243f5e98f9;

# The input vocabulary of sinteract. Every display turns terminal, window
# and browser input into these, and they cross the wire unchanged.
#
# This file is the source of truth for the input format. The same rules as
# scene.capnp apply. See its header for how to regenerate the bindings.

# What happened to a key. The window and a browser send down and then press
# when a key goes down and each time it repeats, and up when it comes up. A
# terminal sends press alone.
enum KeyKind {
    press @0;  # the key typed, when it goes down and each time it repeats
    down  @1;  # the key went down or repeats, just before its press
    up    @2;  # the key came up
}

# ----- Input events (view → engine) -----

# The modifier keys held during an event.
struct Modifiers {
    alt   @0 :Bool;
    ctrl  @1 :Bool;
    shift @2 :Bool;
    # The Windows, Command or Super key.
    meta  @3 :Bool;
}

struct KeyEvent {
    kind      @0 :KeyKind;
    # The W3C KeyboardEvent.key value, such as "ArrowLeft", "a" or " ".
    key       @1 :Text;
    modifiers @2 :Modifiers;
    # The key is held and the system repeats it.
    repeat    @3 :Bool;
}

# A reader skips an event whose arm it does not know, and an event that holds
# a value it does not know, such as a key kind.
struct InputEvent {
    union {
        key  @0 :KeyEvent;
        tick @1 :Tick;
    }
}

# The view is ready for the next frame. A struct and not a Void, so that a
# sequence number can join it as a field.
struct Tick {}

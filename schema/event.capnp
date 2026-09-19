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

# The W3C MouseEvent.button numbers.
enum MouseButton {
    left    @0;
    middle  @1;
    right   @2;
    back    @3;
    forward @4;
}

# The primary pointer, in the coordinates of the scene on the screen. A
# point over the margin of a scaled window falls outside the scene.
struct MouseEvent {
    x         @0 :Float32;
    y         @1 :Float32;
    modifiers @2 :Modifiers;
    # The buttons held after this event, one bit (1 << MouseButton) each.
    buttons   @3 :UInt8;
    union {
        move  @4 :Void;
        down  @5 :MouseButton;
        up    @6 :MouseButton;
        # In notches of the wheel. As in W3C, dx > 0 scrolls right and
        # dy > 0 scrolls down.
        wheel :group {
            dx @7 :Float32;
            dy @8 :Float32;
        }
        # The pointer left the surface, and x and y hold its last position.
        leave @9 :Void;
    }
}

# The largest scene that the display shows at scale 1 with no margin, in
# logical pixels.
struct ResizeEvent {
    width  @0 :Float32;
    height @1 :Float32;
}

# A reader skips an event whose arm it does not know, an event that holds a
# value it does not know, such as a key kind, and an event that holds a
# float that is not finite.
struct InputEvent {
    union {
        key    @0 :KeyEvent;
        tick   @1 :Tick;
        mouse  @2 :MouseEvent;
        resize @3 :ResizeEvent;
    }
}

# The view is ready for the next frame. A struct and not a Void, so that a
# sequence number can join it as a field.
struct Tick {}

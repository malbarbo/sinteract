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

# The buttons of a pad, named as in the standard layout of the W3C
# Gamepad API. a is the bottom button of the right cluster, b the right
# one, x the left one and y the top one.
enum PadButton {
    up            @0;
    down          @1;
    left          @2;
    right         @3;
    a             @4;
    b             @5;
    x             @6;
    y             @7;
    leftShoulder  @8;
    rightShoulder @9;
    select        @10;
    start         @11;
}

# The input of a gamepad or of a pad on the screen of the view.
struct PadEvent {
    union {
        down         @0 :PadButton;
        up           @1 :PadButton;
        # The view found a pad, which sends the buttons from now on.
        connected    @2 :Void;
        # The view lost its pad.
        disconnected @3 :Void;
    }
}

# A reader skips an event whose arm it does not know, an event that holds a
# value it does not know, such as a key kind, and an event that holds a
# float that is not finite.
struct InputEvent {
    union {
        key    @0 :KeyEvent;
        mouse  @1 :MouseEvent;
        resize @2 :ResizeEvent;
        pad    @3 :PadEvent;
    }
}

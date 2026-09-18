@0x9f762cd052af55c1;

# The session between the engine, which runs the program, and the view,
# which draws the frames and sends the input. It carries the payloads of
# scene.capnp and event.capnp and adds nothing to them.
#
# Each direction has its own root, EngineMessage from the engine and
# ViewMessage from the view, so neither side can receive a message that
# only it sends.
#
# This file is the source of truth for the session. The same rules as
# scene.capnp apply. See its header for how to regenerate the bindings.

using Draw = import "scene.capnp";
using Input = import "event.capnp";

# A reader skips a message whose arm it does not know. It drops a message
# that does not decode, and the session goes on.

struct AssetMsg {
    id   @0 :UInt32;
    blob @1 :Data;
    # MIME hint such as "image/png". A renderer sniffs the blob when it is
    # empty.
    mime @2 :Text;
}

# Either side ends the session. A struct and not a Void, so that a reason
# can join it as a field.
struct Close {}

# Engine to view.
struct EngineMessage {
    union {
        # One per bitmap, before the frames that draw it.
        asset @0 :AssetMsg;
        # One per repaint.
        frame @1 :Draw.Scene;
        close @2 :Close;
    }
}

# View to engine.
struct ViewMessage {
    union {
        event @0 :Input.InputEvent;
        close @1 :Close;
    }
}

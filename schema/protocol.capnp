@0x9f762cd052af55c1;

# The session between the engine, which runs the program, the views,
# which draw the frames and send the input, and the server, which owns the
# session. It carries the payloads of scene.capnp and event.capnp and adds
# nothing to them.
#
# Each writer has its own root, EngineMessage from the engine,
# ViewMessage from a view and ServerMessage from the server, so no side can
# receive a message that only another side sends. The engine reads the
# server, or the one view when no server sits between them.
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

# View to server, or to the engine when no server sits between them.
struct ViewMessage {
    union {
        event @0 :Input.InputEvent;
        close @1 :Close;
    }
}

# A player of the session.
struct Member {
    # The number of the player in the session, from 1, which the header of
    # a message about this player carries. A player is in the members of a
    # start once.
    player   @0 :UInt32;
    nickname @1 :Text;
}

# The players when the session starts.
struct Start {
    members @0 :List(Member);
}

# The player in the header, from 1, joined the session.
struct Join {
    nickname @0 :Text;
}

# The player in the header, from 1, left the session. A struct and not a Void, so
# that a reason can join it as a field.
struct Leave {}

# Server to engine.
struct ServerMessage {
    union {
        # The input of the player in the header, from 1.
        event @0 :Input.InputEvent;
        close @1 :Close;
        # The first message of the session.
        start @2 :Start;
        join  @3 :Join;
        leave @4 :Leave;
    }
}

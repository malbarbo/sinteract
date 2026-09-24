@0x9f762cd052af55c1;

# The session between the engine, which runs the program, the views,
# which draw the frames and send the input, and the server, which owns the
# session. It carries the payloads of scene.capnp and event.capnp, and adds
# the player to a frame, and to the event, the join and the leave of the
# server.
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

# A repaint for one player or for all of them.
struct Frame {
    # The player that the frame goes to, from 1, or 0 for every player.
    player @0 :UInt32;
    scene  @1 :Draw.Scene;
}

# Engine to view. An asset and a close go to every player.
struct EngineMessage {
    union {
        # One per bitmap, before the frames that draw it.
        asset @0 :AssetMsg;
        # One per repaint.
        frame @1 :Frame;
        close @2 :Close;
    }
}

# View to server, or to the engine when no server sits between them. The
# server knows the player of a view from its connection, so a message of a
# view names no player.
struct ViewMessage {
    union {
        event @0 :Input.InputEvent;
        close @1 :Close;
    }
}

# A player of the session.
struct Member {
    # The number of the player in the session, from 1, which a message
    # about this player carries. A player is in the members of a start
    # once.
    player   @0 :UInt32;
    nickname @1 :Text;
}

# The players when the session starts.
struct Start {
    members @0 :List(Member);
}

# The input of a player.
struct PlayerEvent {
    # The player, from 1.
    player @0 :UInt32;
    event  @1 :Input.InputEvent;
}

# A player joined the session.
struct Join {
    # The player, from 1.
    player   @0 :UInt32;
    nickname @1 :Text;
}

# A player left the session.
struct Leave {
    # The player, from 1.
    player @0 :UInt32;
}

# Server to engine. A close and a start are about the whole session.
struct ServerMessage {
    union {
        event @0 :PlayerEvent;
        close @1 :Close;
        # The first message of the session.
        start @2 :Start;
        join  @3 :Join;
        leave @4 :Leave;
    }
}

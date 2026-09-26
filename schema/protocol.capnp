@0x9f762cd052af55c1;

# The session between the engine, which runs the program, the views,
# which draw the frames and send the input, and the server, which owns the
# session. It carries the payloads of scene.capnp and event.capnp, and adds
# the player to a frame and to the event of the server.
#
# Each writer has its own root, EngineMessage from the engine,
# ViewMessage from a view and ServerMessage from the server, so no side can
# receive a message that only another side sends. The engine reads the
# server.
#
# This file is the source of truth for the session. The same rules as
# scene.capnp apply. See its header for how to regenerate the bindings.

using Draw = import "scene.capnp";
using Input = import "event.capnp";

# A reader skips a message whose arm it does not know. It drops a message
# that does not decode, and the session goes on.
#
# No message ends the session. The end of the stream does, the end of a
# pipe or the close of a WebSocket, which also covers a writer that
# crashes.

# The format of the image comes from the first bytes of the blob.
struct AssetMsg {
    id   @0 :UInt32;
    blob @1 :Data;
}

# A repaint for one player or for all of them.
struct Frame {
    # The player that the frame goes to, from 1, or 0 for every player.
    player @0 :UInt32;
    scene  @1 :Draw.Scene;
}

# The players that a game takes, with 1 <= minPlayers <= maxPlayers.
struct Hello {
    minPlayers @0 :UInt32;
    maxPlayers @1 :UInt32;
}

# Engine to view, through the server. An asset goes to every player. The
# engine sends an asset before the first frame that draws it, and the
# server keeps it for the views while the room has room for it.
struct EngineMessage {
    union {
        # One per bitmap, before the frames that draw it.
        asset @0 :AssetMsg;
        # One per repaint.
        frame @1 :Frame;
        # The first message of the engine, and only once. It goes to the
        # server, which starts the session if the game takes its players.
        hello @2 :Hello;
        # From the server to a view, which drops the asset of this id,
        # since no frame of the view draws it. An engine never sends it.
        forget @3 :UInt32;
        # To the server, as the engine reads a tick. The server sends the
        # next tick only after it, so the ticks of an engine slower than
        # the timer do not pile up.
        tickTaken @4 :Void;
    }
}

# View to server. The server knows the player of a view from its
# connection, so a message of a view names no player. A second kind of
# message makes `event` the first arm of a new union, which Cap'n Proto
# allows for a field that is alone in the union. A reader skips a message
# with no event.
struct ViewMessage {
    event @0 :Input.InputEvent;
}

# A player of the session.
struct Member {
    # The number of the player in the session, from 1, which a message
    # about this player carries. A player is in the members of a start
    # once.
    player   @0 :UInt32;
    nickname @1 :Text;
}

# The players of the session, who are the same until its end.
struct Start {
    members @0 :List(Member);
}

# The input of a player.
struct PlayerEvent {
    # The player, from 1.
    player @0 :UInt32;
    event  @1 :Input.InputEvent;
}

# Server to engine. A start and a tick are about the whole session.
struct ServerMessage {
    union {
        # The input of a player, never a tick, since the server paces the
        # engine for every player.
        event @0 :PlayerEvent;
        # The first message of the session.
        start @1 :Start;
        # Time for the engine to draw the next frames.
        tick  @2 :Input.Tick;
        # The server dropped the asset of this id, to keep the room under
        # its limits. The engine sends the image again, under a new id,
        # before a frame that draws it.
        lost  @3 :UInt32;
    }
}

@0x9f762cd052af55c1;

# The session between the engine, which runs the program, the views,
# which draw the frames and send the input, and the server, which owns the
# session. It carries the payloads of scene.capnp and event.capnp, and adds
# the player to a frame and to the event of the server.
#
# Each direction has its own root, named after it, EngineToServer,
# ServerToEngine, ViewToServer and ServerToView, so no side can receive a
# message that only another side sends. The first message of the engine
# has the root Hello, and the first message of the server has the root
# Start. Neither comes again, so neither is an arm of a union.
#
# This file is the source of truth for the session. The same rules as
# scene.capnp apply. See its header for how to regenerate the bindings.

using Input = import "event.capnp";

# A reader skips a message whose arm it does not know. It drops a message
# that does not decode, and the session goes on.
#
# No message ends the session. The end of the stream does, the end of a
# pipe or the close of a WebSocket, which also covers a writer that
# crashes.

# The format of the image comes from the first bytes of the blob.
struct Asset {
    id   @0 :UInt32;
    blob @1 :Data;
}

# A repaint for one player or for all of them.
struct Frame {
    # The player that the frame goes to, from 1, or 0 for every player.
    player @0 :UInt32;
    # A whole message whose root is the Scene of scene.capnp. The server
    # copies the bytes into the frame of a ServerToView with no decode.
    scene  @1 :Data;
}

# The root of the first message of the engine, and of no other. It gives
# the players that the game takes, with 1 <= minPlayers <= maxPlayers <=
# 1024, and the server starts the session if the game takes its players.
struct Hello {
    minPlayers @0 :UInt32;
    maxPlayers @1 :UInt32;
}

# Engine to server, after the hello. The engine sends an asset before the
# first frame that draws it, and the server keeps it for the views while
# the room has room for it.
struct EngineToServer {
    union {
        # One per bitmap, before the frames that draw it.
        asset @0 :Asset;
        # One per repaint.
        frame @1 :Frame;
        # To the server, as the engine reads a tick. The server sends the
        # next tick only after it, so the ticks of an engine slower than
        # the timer do not pile up.
        tickTaken @2 :Void;
    }
}

# View to server. The server knows the player of a view from its
# connection, so a message of a view names no player. A second kind of
# message makes `event` the first arm of a new union, which Cap'n Proto
# allows for a field that is alone in the union. A reader skips a message
# with no event.
struct ViewToServer {
    event @0 :Input.InputEvent;
}

# A player of the session. The number of the player, which a message
# about the player carries, is its place in the members of the start,
# from 1.
struct Member {
    nickname @0 :Text;
}

# The root of the first message of the server, and of no other. It gives
# the players of the session, who are the same until its end.
struct Start {
    members @0 :List(Member);
}

# The input of a player.
struct PlayerInput {
    # The player, from 1.
    player @0 :UInt32;
    event  @1 :Input.InputEvent;
}

# Time for the engine to draw the next frames. A struct and not a Void, so
# that a sequence number can join it as a field.
struct Tick {}

# Server to engine, after the start. A tick is about the whole session.
struct ServerToEngine {
    union {
        # The input of a player.
        input @0 :PlayerInput;
        # Time for the engine to draw the next frames.
        tick  @1 :Tick;
        # The server dropped the asset of this id, to keep the room under
        # its limits. The engine sends the image again, under a new id,
        # before a frame that draws it.
        lost  @2 :UInt32;
    }
}

# Server to view. The server copies an asset and the scene of a frame from
# the engine, and sends a view only the assets that its frames draw.
struct ServerToView {
    union {
        # Before the first frame of the view that draws it.
        asset  @0 :Asset;
        # A whole message whose root is the Scene of scene.capnp, as the
        # frame of the engine holds it.
        frame  @1 :Data;
        # The view drops the asset of this id, since no frame of the view
        # draws it.
        forget @2 :UInt32;
    }
}

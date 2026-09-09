@0x9f762cd052af55c1;

# The session between the server, which runs the engine, and the client,
# which renders. It carries the payloads of scene.capnp and event.capnp and
# adds nothing to them.
#
# This file is the source of truth for the session. The same rules as
# scene.capnp apply. See its header for how to regenerate the bindings.

using Draw = import "scene.capnp";
using Input = import "event.capnp";

# ----- Top-level message envelope -----
#
# Server to client:
#   * asset        (one per bitmap, before the frames)
#   * frame        (one per repaint)
#   * sessionClose
#
# Client to server:
#   * event
#   * sessionClose

struct AssetMsg {
    id   @0 :UInt32;
    blob @1 :Data;
    # MIME hint such as "image/png". A renderer sniffs the blob when it is
    # empty.
    mime @2 :Text;
}

struct Message {
    union {
        asset        @0 :AssetMsg;
        frame        @1 :Draw.Scene;
        event        @2 :Input.InputEvent;
        sessionClose @3 :Void;
    }
}

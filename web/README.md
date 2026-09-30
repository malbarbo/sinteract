# The HTML client of a view

This directory holds the view of a session for a browser, written in
TypeScript and built with Deno into one HTML file, `dist/index.html`. The
page opens a WebSocket to the server, draws each frame on a canvas and sends
the input of the player back. `SERVER.md`, at the root of the repository,
is the contract with the server.

`src/capnp/` holds the code that capnp-es generates from `../schema`, and
`make capnp` generates it again after a change of the schema. The files are
committed, so a build needs only Deno. `src/scene.ts` turns the bytes of a
frame into plain values, with the rules of the Rust reader for a value it
does not know. `src/render.ts` draws those values with the Canvas 2D API,
`src/input.ts` turns the keys, the pointer, the wheel, the size of the page
and a gamepad into the events of the protocol, and `src/protocol.ts`
encodes and decodes the messages. `src/view.ts` ties them together behind
one class, and `src/main.ts` is the page of a player, which takes its
token from `?token=` and connects to `/play` of the same server:

```ts
const view = new View(canvas, { onStatus: (s) => console.log(s.kind) });
view.connect(`wss://${location.host}/play?token=${token}`);
```

The text uses the Liberation fonts when the system has them, and Arial,
Times New Roman and Courier New otherwise, which have the same metrics, so
a text measures as it does in the engine.

`server/` is a server to try the client with, with one room and no lobby.
It starts the room as soon as the engine sends its hello and prints the
link of each player, so two tabs play the same game:

```sh
make -C web
cargo build --release --examples
cargo run --release --manifest-path web/server/Cargo.toml -- \
    --players 2 target/release/examples/engine 20
```

`make check` checks the format, the lint and the types, and `make test`
runs the tests.

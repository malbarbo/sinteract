# The HTML client of a view

This directory holds the view of a session for a browser, written in TypeScript
and built with Deno. The page opens a WebSocket to the server, draws each frame
on a canvas and sends the input of the player back. `SERVER.md`, at the root of
the repository, is the contract with the server.

The client comes in two pages, each one HTML file. `dist/index.html` reads and
draws the frames in TypeScript. `dist/rust.html` reads them with the
`FrameReader` of the crate and draws them with its canvas renderer, compiled to
WebAssembly from `wasm/`, and draws the text with the glyphs of the engine. The
two share the rest of the page, and `make` builds both. The Rust page needs
`wasm-pack` and carries the module in base64, so it weighs about 3.0 MB, against
about 80 KB for the TypeScript page.

`src/capnp/` holds the code that capnp-es generates from `../schema`, and
`make capnp` generates it again after a change of the schema. The files are
committed, so a build needs only Deno. `src/scene.ts` turns the bytes of a frame
into plain values, with the rules of the Rust reader for a value it does not
know. It reads the frame with `src/reader.ts`, a reader of the wire format that
reads each field at its place in the layout. The accessors of capnp-es take
about 5 ms to read a frame of 200 paths, and this reader takes under 1 ms.
`src/render.ts` draws those values with the Canvas 2D API, `src/input.ts` turns
the keys, the pointer, the wheel, the size of the page and a gamepad into the
events of the protocol, and `src/protocol.ts` encodes and decodes the messages.
`src/screen.ts` holds `TsScreen`, which reads the messages of the server into
those values and draws them, behind the `Screen` interface that the Rust page
also implements. `src/view.ts` ties them together behind one class, and
`src/page.ts` is the page of a player, which takes its token from `?token=` and
connects to `/play` of the same server. `src/main.ts` and `src/rust.ts` start it
with each screen:

```ts
const view = new View(canvas, { onStatus: (s) => console.log(s.kind) });
view.connect(`wss://${location.host}/play?token=${token}`);
```

The engine embeds the Sinteract fonts, which have the outlines of the Liberation
fonts. The TypeScript page draws the text with the Liberation fonts when the
system has them, and with Arial, Times New Roman and Courier New otherwise,
which have the same metrics, so a text measures as it does in the engine.

`server/` is a server to try the client with, with one room and no lobby. It
starts the room as soon as the engine sends its hello and prints the link of
each player, so two tabs play the same game:

```sh
make -C web
cargo build --release --examples
cargo run --release --manifest-path web/server/Cargo.toml -- \
    --players 2 target/release/examples/engine 20
```

The test server serves `dist/index.html`, and `--page web/dist/rust.html` serves
the Rust page.

`make check` checks the format, the lint and the types, and `make test` runs the
tests. `make render-test` draws the gallery of `examples/gallery.rs` with each
page in a headless Chrome, through the test server, and compares it with the PNG
of the pixmap renderer. The text and the antialiasing of Chrome never match
tiny-skia pixel by pixel, so the test compares the mean difference of each tile
of 50 by 50 pixels. It looks for Chromium or Google Chrome on the path, and
`make render-test CHROME=...` names another one. The screenshots of Chrome go to
`build/gallery-index.png` and `build/gallery-rust.png`.

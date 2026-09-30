import * as $ from "capnp-es";
import { assertEquals } from "@std/assert";
import * as E from "../src/capnp/event.ts";
import * as P from "../src/capnp/protocol.ts";
import { encodeInput, readServerMessage } from "../src/protocol.ts";

const NONE = { alt: false, ctrl: false, shift: false, meta: false };

function readEvent(bytes: ArrayBuffer): E.InputEvent {
  return new $.Message(bytes, false).getRoot(P.ViewToServer).event;
}

Deno.test("a key goes out with its kind, its key and its modifiers", () => {
  const e = readEvent(encodeInput({
    kind: "key",
    type: "up",
    key: "ArrowLeft",
    modifiers: { ...NONE, shift: true },
  }));
  assertEquals(e.which(), E.InputEvent.KEY);
  assertEquals([e.key.kind, e.key.key, e.key.modifiers.shift], [
    E.KeyKind.UP,
    "ArrowLeft",
    true,
  ]);
});

Deno.test("a wheel of the mouse goes out with its notches and its buttons", () => {
  const e = readEvent(encodeInput({
    kind: "mouse",
    x: 1.5,
    y: 2,
    modifiers: NONE,
    buttons: 4,
    action: { kind: "wheel", dx: 0, dy: -1 },
  }));
  const m = e.mouse;
  assertEquals(m.which(), E.MouseEvent.WHEEL);
  assertEquals([m.x, m.y, m.buttons, m.wheel.dx, m.wheel.dy], [
    1.5,
    2,
    4,
    0,
    -1,
  ]);
});

Deno.test("a pad and a resize go out in their arms", () => {
  const pad = readEvent(encodeInput({
    kind: "pad",
    action: { kind: "down", button: E.PadButton.START },
  }));
  assertEquals([pad.pad.which(), pad.pad.down], [
    E.PadEvent.DOWN,
    E.PadButton.START,
  ]);
  const resize = readEvent(
    encodeInput({ kind: "resize", width: 640, height: 480 }),
  );
  assertEquals([resize.resize.width, resize.resize.height], [640, 480]);
});

function serverMessage(write: (m: P.ServerToView) => void): ArrayBuffer {
  const message = new $.Message();
  write(message.initRoot(P.ServerToView));
  return message.toArrayBuffer();
}

Deno.test("the server sends an asset, a frame and a forget", () => {
  const asset = readServerMessage(serverMessage((m) => {
    const a = m._initAsset();
    a.id = 7;
    a._initBlob(3).copyBuffer(new Uint8Array([1, 2, 3]));
  }));
  assertEquals(asset, {
    kind: "asset",
    id: 7,
    blob: new Uint8Array([1, 2, 3]),
  });
  const forget = readServerMessage(serverMessage((m) => (m.forget = 7)));
  assertEquals(forget, { kind: "forget", id: 7 });
  const frame = readServerMessage(serverMessage((m) => {
    m._initFrame(8).copyBuffer(new Uint8Array(8).fill(5));
  }));
  assertEquals(frame, { kind: "frame", scene: new Uint8Array(8).fill(5) });
});

Deno.test("a message of an unknown arm reads as null", () => {
  const bytes = serverMessage((m) => {
    m.forget = 1;
    $.utils.setUint16(0, 99, m);
  });
  assertEquals(readServerMessage(bytes), null);
});

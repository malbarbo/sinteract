// The messages between a view and the server, with no envelope, since the
// WebSocket frames each message itself. The server sends a ServerToView and
// the view sends a ViewToServer, both of schema/protocol.capnp.

import * as $ from "capnp-es";
import * as E from "./capnp/event.ts";
import * as P from "./capnp/protocol.ts";

// The subprotocol of the WebSocket, which names the version of the
// messages, as view::SUBPROTOCOL in Rust.
export const SUBPROTOCOL = "sinteract.v1";

export type ServerMessage =
  // An image, PNG, JPEG, GIF or WebP, that the frames draw by its id.
  | { kind: "asset"; id: number; blob: Uint8Array }
  // A whole message whose root is a Scene, for decodeScene.
  | { kind: "frame"; scene: Uint8Array }
  // No frame of the view draws the asset of this id anymore.
  | { kind: "forget"; id: number };

// Reads a message of the server, or returns `null` for a message of an arm
// from a newer schema. Throws for a message that does not decode.
export function readServerMessage(payload: ArrayBuffer): ServerMessage | null {
  const m = new $.Message(payload, false).getRoot(P.ServerToView);
  switch (m.which()) {
    case P.ServerToView.ASSET: {
      const a = m.asset;
      // A copy, since the blob outlives the message.
      return { kind: "asset", id: a.id, blob: a.blob.toUint8Array().slice() };
    }
    case P.ServerToView.FRAME:
      return { kind: "frame", scene: m.frame.toUint8Array() };
    case P.ServerToView.FORGET:
      return { kind: "forget", id: m.forget };
    default:
      return null;
  }
}

// The input vocabulary of schema/event.capnp.
export type InputEvent =
  | { kind: "key"; type: KeyType; key: string; modifiers: Modifiers }
  | {
    kind: "mouse";
    x: number;
    y: number;
    modifiers: Modifiers;
    // The buttons held after this event, one bit (1 << MouseButton) each.
    buttons: number;
    action: MouseAction;
  }
  // The largest scene that the view shows at scale 1 with no margin.
  | { kind: "resize"; width: number; height: number }
  | { kind: "pad"; action: PadAction };

// A browser sends down and then press when a key goes down and each time it
// repeats, and up when it comes up.
export type KeyType = "press" | "down" | "up";

export interface Modifiers {
  alt: boolean;
  ctrl: boolean;
  shift: boolean;
  // The Windows, Command or Super key.
  meta: boolean;
}

// The W3C MouseEvent.button numbers.
export const MouseButton = E.MouseButton;
export type MouseButton = E.MouseButton;

export type MouseAction =
  | { kind: "move" }
  | { kind: "down"; button: MouseButton }
  | { kind: "up"; button: MouseButton }
  // In notches of the wheel. dx > 0 scrolls right and dy > 0 scrolls down.
  | { kind: "wheel"; dx: number; dy: number }
  // The pointer left the canvas, and x and y hold its last position.
  | { kind: "leave" };

// The buttons of the standard layout of the W3C Gamepad API.
export const PadButton = E.PadButton;
export type PadButton = E.PadButton;

export type PadAction =
  | { kind: "down"; button: PadButton }
  | { kind: "up"; button: PadButton }
  | { kind: "connected" }
  | { kind: "disconnected" };

// Encodes `event` as a message of the view.
export function encodeInput(event: InputEvent): ArrayBuffer {
  const message = new $.Message();
  const e = message.initRoot(P.ViewToServer)._initEvent();
  switch (event.kind) {
    case "key": {
      const k = e._initKey();
      k.kind = KEY_KINDS[event.type];
      k.key = event.key;
      writeModifiers(k._initModifiers(), event.modifiers);
      break;
    }
    case "mouse": {
      const m = e._initMouse();
      m.x = event.x;
      m.y = event.y;
      m.buttons = event.buttons;
      writeModifiers(m._initModifiers(), event.modifiers);
      const action = event.action;
      switch (action.kind) {
        case "move":
          m.move = true;
          break;
        case "down":
          m.down = action.button;
          break;
        case "up":
          m.up = action.button;
          break;
        case "wheel": {
          const w = m._initWheel();
          w.dx = action.dx;
          w.dy = action.dy;
          break;
        }
        case "leave":
          m.leave = true;
          break;
      }
      break;
    }
    case "resize": {
      const r = e._initResize();
      r.width = event.width;
      r.height = event.height;
      break;
    }
    case "pad": {
      const p = e._initPad();
      const action = event.action;
      switch (action.kind) {
        case "down":
          p.down = action.button;
          break;
        case "up":
          p.up = action.button;
          break;
        case "connected":
          p.connected = true;
          break;
        case "disconnected":
          p.disconnected = true;
          break;
      }
      break;
    }
  }
  return message.toArrayBuffer();
}

const KEY_KINDS: Record<KeyType, E.KeyKind> = {
  press: E.KeyKind.PRESS,
  down: E.KeyKind.DOWN,
  up: E.KeyKind.UP,
};

function writeModifiers(out: E.Modifiers, m: Modifiers): void {
  out.alt = m.alt;
  out.ctrl = m.ctrl;
  out.shift = m.shift;
  out.meta = m.meta;
}

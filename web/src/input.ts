// Turns the input of the browser into the InputEvent of the protocol: the
// keys, the primary pointer over the canvas, the wheel, the size of the
// canvas and the first gamepad.

import {
  type InputEvent,
  type Modifiers,
  type MouseAction,
  MouseButton,
  PadButton,
} from "./protocol.ts";

// The point of the scene under a point of the canvas, in CSS pixels from its
// top left corner.
export type ToScene = (x: number, y: number) => [number, number];

// Listens to the input on `canvas` and on its window, and passes each event
// to `send`. Returns the function that stops the listening.
export function listen(
  canvas: HTMLCanvasElement,
  toScene: ToScene,
  send: (event: InputEvent) => void,
): () => void {
  const controller = new AbortController();
  const signal = controller.signal;
  // The key that went down with each physical key, so the up names the key
  // that the down named even when a modifier changed in between.
  const held = new Map<string, string>();
  const releaseKeys = () => {
    for (const key of held.values()) {
      send({ kind: "key", type: "up", key, modifiers: NO_MODIFIERS });
    }
    held.clear();
  };

  globalThis.addEventListener("keydown", (e) => {
    if (!claims(e)) return;
    e.preventDefault();
    const modifiers = modifiersOf(e);
    held.set(e.code, e.key);
    send({ kind: "key", type: "down", key: e.key, modifiers });
    send({ kind: "key", type: "press", key: e.key, modifiers });
  }, { signal });
  globalThis.addEventListener("keyup", (e) => {
    const key = held.get(e.code);
    if (key === undefined) return;
    e.preventDefault();
    held.delete(e.code);
    send({ kind: "key", type: "up", key, modifiers: modifiersOf(e) });
  }, { signal });
  // A key that comes up while the page has no focus sends no keyup, so the
  // engine gets the up of every held key when the focus goes.
  globalThis.addEventListener("blur", releaseKeys, { signal });

  // The buttons of the last mouse event, as the engine knows them.
  let mouseButtons = 0;
  const mouse = (
    e: PointerEvent | WheelEvent,
    action: MouseAction,
    buttons = buttonsOf(e.buttons),
  ) => {
    const rect = canvas.getBoundingClientRect();
    const [x, y] = toScene(e.clientX - rect.left, e.clientY - rect.top);
    mouseButtons = buttons;
    send({ kind: "mouse", x, y, modifiers: modifiersOf(e), buttons, action });
  };
  // Sends the down or the up of the button of `e`, from its bit in
  // `e.buttons`.
  const press = (e: PointerEvent) => {
    const button = buttonOf(e.button);
    if (button === null) return;
    const down = (buttonsOf(e.buttons) & (1 << button)) !== 0;
    mouse(e, { kind: down ? "down" : "up", button });
  };
  canvas.addEventListener("pointermove", (e) => {
    if (!e.isPrimary) return;
    // A button that goes down or up while another one is held comes as a
    // move with its button set.
    if (e.button === -1) mouse(e, { kind: "move" });
    else press(e);
  }, { signal });
  canvas.addEventListener("pointerdown", (e) => {
    if (!e.isPrimary) return;
    canvas.focus();
    // The up comes to the canvas even outside it.
    canvas.setPointerCapture(e.pointerId);
    press(e);
  }, { signal });
  canvas.addEventListener("pointerup", (e) => {
    if (e.isPrimary) press(e);
  }, { signal });
  // A pointer that the system cancels, as for a gesture of the system,
  // sends no pointerup, so the engine gets the up of each held button.
  canvas.addEventListener("pointercancel", (e) => {
    if (!e.isPrimary) return;
    for (let button = 0; button <= MouseButton.FORWARD; button++) {
      const bit = 1 << button;
      if (mouseButtons & bit) {
        mouse(
          e,
          { kind: "up", button: button as MouseButton },
          mouseButtons & ~bit,
        );
      }
    }
  }, { signal });
  canvas.addEventListener("pointerleave", (e) => {
    if (e.isPrimary) mouse(e, { kind: "leave" });
  }, { signal });
  canvas.addEventListener("wheel", (e) => {
    e.preventDefault();
    const notches = WHEEL_NOTCH[e.deltaMode] ?? 1;
    mouse(e, { kind: "wheel", dx: e.deltaX * notches, dy: e.deltaY * notches });
  }, { signal, passive: false });
  // The right button belongs to the game.
  canvas.addEventListener("contextmenu", (e) => e.preventDefault(), { signal });

  const resize = () => {
    const rect = canvas.getBoundingClientRect();
    send({ kind: "resize", width: rect.width, height: rect.height });
  };
  const observer = new ResizeObserver(resize);
  observer.observe(canvas);

  const pad = new Pad(send);
  signal.addEventListener("abort", () => {
    observer.disconnect();
    pad.stop();
  });
  return () => controller.abort();
}

// Returns `true` if the game takes `e`, `false` otherwise. A key with Ctrl or
// Meta, and a function key, stay with the browser, so a reload, a zoom or
// the tools of the developer still work.
function claims(e: KeyboardEvent): boolean {
  return !(e.ctrlKey || e.metaKey || /^F\d+$/.test(e.key));
}

const NO_MODIFIERS: Modifiers = {
  alt: false,
  ctrl: false,
  shift: false,
  meta: false,
};

function modifiersOf(e: KeyboardEvent | MouseEvent): Modifiers {
  return { alt: e.altKey, ctrl: e.ctrlKey, shift: e.shiftKey, meta: e.metaKey };
}

function buttonOf(button: number): MouseButton | null {
  return button >= 0 && button <= MouseButton.FORWARD
    ? button as MouseButton
    : null;
}

// MouseEvent.buttons has the bits left, right, middle, back and forward,
// and the protocol has one bit per MouseButton, in the order of
// MouseEvent.button, which puts middle before right.
function buttonsOf(buttons: number): number {
  const bit = (from: number, to: MouseButton) => (buttons & from ? 1 << to : 0);
  return bit(1, MouseButton.LEFT) | bit(2, MouseButton.RIGHT) |
    bit(4, MouseButton.MIDDLE) | bit(8, MouseButton.BACK) |
    bit(16, MouseButton.FORWARD);
}

// Notches of the wheel for one unit of each WheelEvent.deltaMode, pixels,
// lines and pages. A browser scrolls 100 pixels or 3 lines for a notch.
const WHEEL_NOTCH = [1 / 100, 1 / 3, 1];

// The buttons of the standard mapping of the Gamepad API, by index.
const PAD_BUTTONS: [number, PadButton][] = [
  [0, PadButton.A],
  [1, PadButton.B],
  [2, PadButton.X],
  [3, PadButton.Y],
  [4, PadButton.LEFT_SHOULDER],
  [5, PadButton.RIGHT_SHOULDER],
  [8, PadButton.SELECT],
  [9, PadButton.START],
  [12, PadButton.DPAD_UP],
  [13, PadButton.DPAD_DOWN],
  [14, PadButton.DPAD_LEFT],
  [15, PadButton.DPAD_RIGHT],
];

// The first gamepad of the standard mapping. The Gamepad API has no event
// for a button, so the pad reads the buttons at each animation frame.
class Pad {
  #send: (event: InputEvent) => void;
  #index: number | null = null;
  #pressed = new Set<PadButton>();
  #frame = 0;

  constructor(send: (event: InputEvent) => void) {
    this.#send = send;
    this.#frame = requestAnimationFrame(this.#poll);
  }

  stop(): void {
    cancelAnimationFrame(this.#frame);
  }

  #poll = () => {
    this.#frame = requestAnimationFrame(this.#poll);
    const pads = navigator.getGamepads?.() ?? [];
    let pad = this.#index === null ? null : pads[this.#index];
    if (this.#index !== null && !pad?.connected) {
      this.#index = null;
      this.#pressed.clear();
      this.#send({ kind: "pad", action: { kind: "disconnected" } });
    }
    if (this.#index === null) {
      pad = pads.find((p) => p?.connected && p.mapping === "standard") ?? null;
      if (!pad) return;
      this.#index = pad.index;
      this.#send({ kind: "pad", action: { kind: "connected" } });
    }
    if (!pad) return;
    for (const [index, button] of PAD_BUTTONS) {
      const down = pad.buttons[index]?.pressed ?? false;
      if (down === this.#pressed.has(button)) continue;
      if (down) this.#pressed.add(button);
      else this.#pressed.delete(button);
      this.#send({
        kind: "pad",
        action: { kind: down ? "down" : "up", button },
      });
    }
  };
}

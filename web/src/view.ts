// A view of a session: it opens the WebSocket to the server, hands each
// message to a Screen, which draws the newest frame on a canvas, and sends
// the input of the user. A page that embeds a view needs only this module
// and the Screen that it picks.

import { listen } from "./input.ts";
import { encodeInput, SUBPROTOCOL } from "./protocol.ts";
import { type MakeScreen, type Screen, TsScreen } from "./screen.ts";

export type Status =
  | { kind: "connecting" }
  | { kind: "open" }
  // The server closed the WebSocket, or the connection fell.
  | { kind: "closed"; code: number; reason: string };

export interface ViewOptions {
  // Each change of the connection.
  onStatus?: (status: Status) => void;
  // A message or a frame that does not decode. The view drops it and goes
  // on.
  onError?: (error: unknown) => void;
  // The screen of each connection, a TsScreen when none is given.
  screen?: MakeScreen;
}

export class View {
  #canvas: HTMLCanvasElement;
  #options: ViewOptions;
  #conn: Connection | null = null;
  // The size of the canvas in CSS pixels.
  #width = 0;
  #height = 0;
  #frame = 0;

  constructor(canvas: HTMLCanvasElement, options: ViewOptions = {}) {
    this.#canvas = canvas;
    this.#options = options;
    new ResizeObserver(() => this.#resize()).observe(canvas);
    this.#resize();
  }

  // Opens the WebSocket at `url`, such as ws://host/play?token=..., and
  // closes the one before it.
  connect(url: string): void {
    this.close();
    this.#status({ kind: "connecting" });
    const socket = new WebSocket(url, SUBPROTOCOL);
    socket.binaryType = "arraybuffer";
    const make = this.#options.screen ??
      ((canvas, redraw) => new TsScreen(canvas, redraw));
    const conn: Connection = {
      socket,
      stopInput: () => {},
      screen: make(this.#canvas, () => this.#request()),
    };
    socket.onopen = () => {
      this.#status({ kind: "open" });
      conn.stopInput = listen(
        this.#canvas,
        (x, y) => this.#toScene(x, y),
        (e) => {
          if (socket.readyState === WebSocket.OPEN) socket.send(encodeInput(e));
        },
      );
    };
    socket.onmessage = (e) => {
      if (e.data instanceof ArrayBuffer) {
        this.#receive(conn, new Uint8Array(e.data));
      }
    };
    // The last frame stays on the canvas after the server closes.
    socket.onclose = (e) => {
      conn.stopInput();
      this.#status({ kind: "closed", code: e.code, reason: e.reason });
    };
    this.#conn = conn;
  }

  // Closes the WebSocket, stops the input and clears the canvas.
  close(): void {
    const conn = this.#conn;
    if (!conn) return;
    this.#conn = null;
    conn.stopInput();
    conn.socket.onclose = null;
    conn.socket.close();
    conn.screen.clear();
    conn.screen.free();
  }

  #receive(conn: Connection, payload: Uint8Array): void {
    try {
      if (conn.screen.read(payload)) this.#request();
    } catch (e) {
      this.#options.onError?.(e);
    }
  }

  #request(): void {
    if (!this.#frame) this.#frame = requestAnimationFrame(() => this.#draw());
  }

  #draw(): void {
    this.#frame = 0;
    try {
      this.#conn?.screen.draw();
    } catch (e) {
      this.#options.onError?.(e);
    }
  }

  // Sizes the canvas to its pixels on the screen, and draws the frame again.
  #resize(): void {
    const c = this.#canvas;
    const rect = c.getBoundingClientRect();
    this.#width = rect.width;
    this.#height = rect.height;
    const dpr = globalThis.devicePixelRatio || 1;
    const width = Math.max(1, Math.round(rect.width * dpr));
    const height = Math.max(1, Math.round(rect.height * dpr));
    if (c.width === width && c.height === height) return;
    c.width = width;
    c.height = height;
    this.#request();
  }

  // The point of the scene under (x, y), in CSS pixels of the canvas.
  #toScene(x: number, y: number): [number, number] {
    const sx = this.#width > 0 ? this.#canvas.width / this.#width : 1;
    const sy = this.#height > 0 ? this.#canvas.height / this.#height : 1;
    const { scale, x: ox, y: oy } = this.#conn?.screen ??
      { scale: 1, x: 0, y: 0 };
    return [(x * sx - ox) / scale, (y * sy - oy) / scale];
  }

  #status(status: Status): void {
    this.#options.onStatus?.(status);
  }
}

// The state of one WebSocket. A late callback of an old connection changes
// only its own state, which nothing draws any more.
interface Connection {
  socket: WebSocket;
  stopInput: () => void;
  screen: Screen;
}

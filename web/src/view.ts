// A view of a session: it opens the WebSocket to the server, keeps the
// images of the assets, draws the newest frame on a canvas and sends the
// input of the user. This is the only module that knows all the others, so
// a page that embeds a view needs only this one.

import { listen } from "./input.ts";
import { encodeInput, readServerMessage, SUBPROTOCOL } from "./protocol.ts";
import { Renderer } from "./render.ts";
import { decodeScene, type Element, type Scene } from "./scene.ts";

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
}

export class View {
  #canvas: HTMLCanvasElement;
  #renderer: Renderer;
  #options: ViewOptions;
  #conn: Connection | null = null;
  // The size of the canvas in CSS pixels.
  #width = 0;
  #height = 0;
  #frame = 0;

  constructor(canvas: HTMLCanvasElement, options: ViewOptions = {}) {
    this.#canvas = canvas;
    this.#renderer = new Renderer(canvas);
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
    const conn: Connection = {
      socket,
      stopInput: () => {},
      images: new Map(),
      latest: null,
      scene: null,
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
      if (e.data instanceof ArrayBuffer) this.#receive(conn, e.data);
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
    for (const asset of conn.images.values()) asset.image?.close();
    conn.images.clear();
    this.#renderer.clear();
  }

  #receive(conn: Connection, payload: ArrayBuffer): void {
    try {
      const message = readServerMessage(payload);
      switch (message?.kind) {
        case "asset":
          addAsset(conn.images, message.id, message.blob);
          break;
        case "forget":
          conn.images.get(message.id)?.image?.close();
          conn.images.delete(message.id);
          break;
        case "frame":
          this.#decode(conn, message.scene);
          break;
      }
    } catch (e) {
      this.#options.onError?.(e);
    }
  }

  #request(): void {
    if (!this.#frame) this.#frame = requestAnimationFrame(() => this.#draw());
  }

  #draw(): void {
    this.#frame = 0;
    const conn = this.#conn;
    if (!conn) return;
    const scene = conn.scene;
    if (!scene) return;
    this.#renderer.draw(
      scene,
      (id) => conn.images.get(id)?.image ?? undefined,
    );
  }

  // Decodes the frame of `bytes`, which goes on screen once the images that
  // it draws are ready, unless a newer frame comes first.
  #decode(conn: Connection, bytes: Uint8Array): void {
    let scene: Scene;
    try {
      scene = decodeScene(bytes);
    } catch (e) {
      this.#options.onError?.(e);
      return;
    }
    conn.latest = scene;
    const pending: Promise<void>[] = [];
    for (const id of bitmapIds(scene.elements)) {
      const asset = conn.images.get(id);
      if (asset && !asset.image) pending.push(asset.ready);
    }
    if (pending.length === 0) {
      conn.scene = scene;
      this.#request();
      return;
    }
    Promise.all(pending).then(() => {
      if (conn.latest !== scene) return;
      conn.scene = scene;
      this.#request();
    });
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
    const { scale, x: ox, y: oy } = this.#renderer.place;
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
  images: Map<number, Asset>;
  // The newest decoded frame, and the newest one whose images are ready,
  // which is the one on screen.
  latest: Scene | null;
  scene: Scene | null;
}

interface Asset {
  // `null` while the image decodes.
  image: ImageBitmap | null;
  ready: Promise<void>;
}

// The id of each bitmap of `elements`, and of what their clips and layers
// hold.
function* bitmapIds(elements: Element[]): Generator<number> {
  for (const e of elements) {
    if (e.kind === "bitmap") yield e.id;
    else if (e.kind === "clipped" || e.kind === "layer") {
      yield* bitmapIds(e.elements);
    }
  }
}

// Decodes the image of an asset into `images`. createImageBitmap is
// asynchronous, so a frame that draws the image waits until it is ready.
function addAsset(
  images: Map<number, Asset>,
  id: number,
  blob: Uint8Array,
): void {
  images.get(id)?.image?.close();
  const asset: Asset = {
    image: null,
    // The Rust view ignores the color profile of an image, so this one
    // does too.
    ready: createImageBitmap(new Blob([blob as BlobPart]), {
      colorSpaceConversion: "none",
    }).then(
      (image) => {
        if (images.get(id) === asset) asset.image = image;
        else image.close();
      },
      // An asset that is not an image keeps no image, and a bitmap of its
      // id draws nothing, as in the Rust view.
      () => {
        if (images.get(id) === asset) images.delete(id);
      },
    ),
  };
  images.set(id, asset);
}

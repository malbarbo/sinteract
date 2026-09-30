// A view of a session: it opens the WebSocket to the server, keeps the
// images of the assets, draws the newest frame on a canvas and sends the
// input of the user. This is the only module that knows all the others, so
// a page that embeds a view needs only this one.

import { listen } from "./input.ts";
import { encodeInput, readServerMessage, SUBPROTOCOL } from "./protocol.ts";
import { fit, type Placement, Renderer } from "./render.ts";
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
  #socket: WebSocket | null = null;
  #stopInput: (() => void) | null = null;
  #images = new Map<number, Asset>();
  // The newest frame, and the newest one whose images are ready, which is
  // the one on screen.
  #latest: Scene | null = null;
  #scene: Scene | null = null;
  #place: Placement = { scale: 1, x: 0, y: 0 };
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
    socket.onopen = () => {
      this.#status({ kind: "open" });
      this.#stopInput = listen(
        this.#canvas,
        (x, y) => this.#toScene(x, y),
        (e) => {
          if (socket.readyState === WebSocket.OPEN) socket.send(encodeInput(e));
        },
      );
    };
    socket.onmessage = (e) => {
      if (e.data instanceof ArrayBuffer) this.#receive(e.data);
    };
    socket.onclose = (e) => {
      this.#stopInput?.();
      this.#stopInput = null;
      if (this.#socket === socket) this.#socket = null;
      this.#status({ kind: "closed", code: e.code, reason: e.reason });
    };
    this.#socket = socket;
  }

  // Closes the WebSocket and stops the input.
  close(): void {
    this.#stopInput?.();
    this.#stopInput = null;
    const socket = this.#socket;
    this.#socket = null;
    if (socket) {
      socket.onclose = null;
      socket.close();
    }
    for (const asset of this.#images.values()) asset.image?.close();
    this.#images.clear();
    // A frame that still waits for its images sees that it is no longer the
    // latest, and the next connection starts with no scene.
    this.#latest = null;
    this.#scene = null;
    this.#place = { scale: 1, x: 0, y: 0 };
    cancelAnimationFrame(this.#frame);
    this.#frame = 0;
  }

  #receive(payload: ArrayBuffer): void {
    let message;
    try {
      message = readServerMessage(payload);
    } catch (e) {
      this.#options.onError?.(e);
      return;
    }
    switch (message?.kind) {
      case "asset":
        this.#addAsset(message.id, message.blob);
        break;
      case "forget":
        this.#images.get(message.id)?.image?.close();
        this.#images.delete(message.id);
        break;
      case "frame": {
        let scene;
        try {
          scene = decodeScene(message.scene, (id) => this.#images.has(id));
        } catch (e) {
          this.#options.onError?.(e);
          return;
        }
        this.#show(scene);
        break;
      }
    }
  }

  // Decodes the image of an asset. createImageBitmap is asynchronous, so a
  // frame that draws the image waits in #show until it is ready.
  #addAsset(id: number, blob: Uint8Array): void {
    this.#images.get(id)?.image?.close();
    const asset: Asset = {
      image: null,
      ready: createImageBitmap(new Blob([blob as BlobPart])).then(
        (image) => {
          if (this.#images.get(id) === asset) asset.image = image;
          else image.close();
        },
        // An asset that is not an image keeps no image, and a bitmap of its
        // id draws nothing, as in the Rust view.
        () => {
          if (this.#images.get(id) === asset) this.#images.delete(id);
        },
      ),
    };
    this.#images.set(id, asset);
  }

  // Shows `scene` once the images that it draws are ready, unless a newer
  // frame comes first.
  #show(scene: Scene): void {
    this.#latest = scene;
    const pending: Promise<void>[] = [];
    for (const id of bitmapIds(scene.elements)) {
      const asset = this.#images.get(id);
      if (asset && !asset.image) pending.push(asset.ready);
    }
    const ready = () => {
      if (this.#latest !== scene) return;
      this.#scene = scene;
      this.#request();
    };
    if (pending.length === 0) ready();
    else Promise.all(pending).then(ready);
  }

  #request(): void {
    if (!this.#frame) this.#frame = requestAnimationFrame(() => this.#draw());
  }

  #draw(): void {
    this.#frame = 0;
    const scene = this.#scene;
    if (!scene) return;
    const c = this.#canvas;
    this.#place = fit(scene.width, scene.height, c.width, c.height);
    this.#renderer.draw(
      scene,
      this.#place,
      (id) => this.#images.get(id)?.image ?? undefined,
    );
  }

  // Sizes the canvas to its pixels on the screen, and draws the frame again.
  #resize(): void {
    const c = this.#canvas;
    const rect = c.getBoundingClientRect();
    const dpr = globalThis.devicePixelRatio || 1;
    const width = Math.max(1, Math.round(rect.width * dpr));
    const height = Math.max(1, Math.round(rect.height * dpr));
    if (c.width === width && c.height === height) return;
    c.width = width;
    c.height = height;
    if (this.#scene) this.#request();
  }

  // The point of the scene under (x, y), in CSS pixels of the canvas.
  #toScene(x: number, y: number): [number, number] {
    const rect = this.#canvas.getBoundingClientRect();
    const sx = rect.width > 0 ? this.#canvas.width / rect.width : 1;
    const sy = rect.height > 0 ? this.#canvas.height / rect.height : 1;
    const { scale, x: ox, y: oy } = this.#place;
    return [(x * sx - ox) / scale, (y * sy - oy) / scale];
  }

  #status(status: Status): void {
    this.#options.onStatus?.(status);
  }
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

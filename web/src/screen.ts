// What draws the frames of one connection on a canvas. The view hands it
// each message of the server and asks it to draw at an animation frame.
// TsScreen reads and draws in TypeScript, and the Screen of wasm/ in Rust.

import { readServerMessage } from "./protocol.ts";
import { fontToLoad, Renderer } from "./render.ts";
import { decodeScene, type Element, type Scene } from "./scene.ts";

export interface Screen {
  // Reads a message of the server. Returns true if it holds a frame, which
  // the next draw draws. Throws for a message that does not decode.
  read(payload: Uint8Array): boolean;
  // Draws the newest frame, fit to the canvas.
  draw(): void;
  // Clears the canvas.
  clear(): void;
  // Lets go of what the screen keeps, such as the images.
  free(): void;
  // Where the last frame went on the canvas, in the pixels of the canvas.
  readonly scale: number;
  readonly x: number;
  readonly y: number;
}

// Makes the screen of a connection. The screen calls `redraw` when it has
// something new to draw with no message, such as an image that finished
// decoding or a font that finished loading.
export type MakeScreen = (
  canvas: HTMLCanvasElement,
  redraw: () => void,
) => Screen;

export class TsScreen implements Screen {
  #renderer: Renderer;
  #redraw: () => void;
  #images = new Map<number, Asset>();
  // The newest frame, not decoded yet.
  #bytes: Uint8Array | null = null;
  // The newest decoded frame, and the newest one whose images are ready,
  // which is the one on screen.
  #latest: Scene | null = null;
  #scene: Scene | null = null;

  constructor(canvas: HTMLCanvasElement, redraw: () => void) {
    this.#renderer = new Renderer(canvas);
    this.#redraw = redraw;
  }

  get scale(): number {
    return this.#renderer.place.scale;
  }

  get x(): number {
    return this.#renderer.place.x;
  }

  get y(): number {
    return this.#renderer.place.y;
  }

  read(payload: Uint8Array): boolean {
    const message = readServerMessage(payload);
    switch (message?.kind) {
      case "asset":
        addAsset(this.#images, message.id, message.blob);
        return false;
      case "forget":
        this.#images.get(message.id)?.image?.close();
        this.#images.delete(message.id);
        return false;
      case "frame":
        // Only the newest frame decodes, at the next draw, so a hidden tab
        // decodes none.
        this.#bytes = message.scene;
        return true;
      default:
        return false;
    }
  }

  draw(): void {
    const bytes = this.#bytes;
    this.#bytes = null;
    if (bytes) this.#decode(bytes);
    const scene = this.#scene;
    if (!scene) return;
    this.#renderer.draw(
      scene,
      (id) => this.#images.get(id)?.image ?? undefined,
    );
  }

  clear(): void {
    this.#renderer.clear();
  }

  free(): void {
    for (const asset of this.#images.values()) asset.image?.close();
    this.#images.clear();
  }

  // Decodes the frame of `bytes`, which goes on screen once the images and
  // the fonts that it draws are ready, unless a newer frame comes first. It
  // throws for a frame that does not decode, and the frame on screen stays.
  #decode(bytes: Uint8Array): void {
    const scene = decodeScene(bytes);
    this.#latest = scene;
    const pending: Promise<void>[] = [];
    for (const e of leaves(scene.elements)) {
      if (e.kind === "bitmap") {
        const asset = this.#images.get(e.id);
        if (asset && !asset.image) pending.push(asset.ready);
      } else if (e.kind === "text") {
        const ready = fontReady(fontToLoad(e));
        if (ready) pending.push(ready);
      }
    }
    if (pending.length === 0) {
      this.#scene = scene;
      return;
    }
    Promise.all(pending).then(() => {
      if (this.#latest !== scene) return;
      this.#scene = scene;
      this.#redraw();
    });
  }
}

interface Asset {
  // `null` while the image decodes.
  image: ImageBitmap | null;
  ready: Promise<void>;
}

// Each element of `elements` and of what their clips and layers hold, but
// the clips and the layers themselves.
function* leaves(elements: Element[]): Generator<Element> {
  for (const e of elements) {
    if (e.kind === "clipped" || e.kind === "layer") yield* leaves(e.elements);
    else yield e;
  }
}

// The web fonts by the CSS font that loads them, with null for a font that
// loaded. A font that fails to load is null too, and its text draws with
// the next font of its list.
const fonts = new Map<string, Promise<void> | null>();

// The promise of the load of `font`, or null when it is over.
function fontReady(font: string): Promise<void> | null {
  let ready = fonts.get(font);
  if (ready === undefined) {
    const settle = () => {
      fonts.set(font, null);
    };
    ready = document.fonts.load(font).then(settle, settle);
    fonts.set(font, ready);
  }
  return ready;
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

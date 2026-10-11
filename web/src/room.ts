// A room of one player for an engine that runs in the same page, such as an
// engine compiled to WebAssembly in a worker. The room plays the server: it
// reads the stream that the engine writes on fd 4, answers its hello with a
// start, ticks once an animation frame, and writes the input of the view
// to the stream that the engine reads on fd 3. Toward the view it is a Link that
// carries the messages of a server, so a View draws an engine of the page
// as it draws one behind a WebSocket.
//
// One player and one view make most of the rules of a server moot: every
// frame goes to the view, every asset fits, since the engine checks the
// limits of a room before it sends a frame, and no asset is ever lost.

import * as $ from "capnp-es";
import * as P from "./capnp/protocol.ts";
import type { Link } from "./view.ts";

export interface RoomOptions {
  // Writes the bytes of the stream that the engine reads, whole messages
  // only. The room never writes part of one.
  write(bytes: Uint8Array): void;
  // The nickname of the player in the start.
  nickname?: string;
  // The rate of the ticks in the start, in thousandths of a hertz, 60000
  // when none is given. The room ticks at each animation frame, so the
  // rate is nominal, and an engine that keeps time measures it.
  tickRate?: number;
  // A stream of the engine that breaks the protocol. The room ends.
  onError?: (error: unknown) => void;
  // The animation frames that pace the ticks. A host that has more to do
  // in a frame, such as reading the stream of the engine before the tick,
  // wraps requestAnimationFrame here.
  requestFrame?: (callback: (now: number) => void) => number;
  cancelFrame?: (handle: number) => void;
}

// Waiting for the hello of the engine, started, or over for good.
type State = "hello" | "open" | "ended";

// The magic of each side, as framing::Side::magic in Rust.
const ENGINE_MAGIC = [0x53, 0x49, 0x45, 0x31]; // SIE1
const SERVER_MAGIC = [0x53, 0x49, 0x53, 0x31]; // SIS1
const HEADER_BYTES = 8;
// The cap of framing.rs, so a corrupted length cannot hold the room forever.
const MAX_MESSAGE_BYTES = 64 * 1024 * 1024;

export class Room {
  #options: RoomOptions;
  // The bytes of the engine that make no whole message yet.
  #pending = new Chunks();
  #state: State = "hello";
  // Whether the engine took the last tick, so the next one can go.
  #tickTaken = true;
  #frame = 0;
  readonly link: Link;

  constructor(options: RoomOptions) {
    this.#options = options;
    this.link = {
      send: (payload) => this.#input(payload),
      close: () => this.#stop(),
      onopen: null,
      onmessage: null,
      onclose: null,
    };
  }

  // Takes the next bytes of the stream of the engine, in any pieces.
  receive(bytes: Uint8Array): void {
    if (this.#state === "ended") return;
    this.#pending.push(bytes);
    let message;
    while ((message = this.#split()) !== null) {
      try {
        this.#read(message);
      } catch (e) {
        this.#options.onError?.(e);
      }
      // Read through a method, since TypeScript keeps the narrowing of the
      // check above across the call that ends the room.
      if (this.#ended()) return;
    }
  }

  #ended(): boolean {
    return this.#state === "ended";
  }

  // The engine is gone, and the view keeps its last frame.
  end(): void {
    if (this.#state === "ended") return;
    this.#stop();
    this.link.onclose?.(1000, "");
  }

  // The first message of the stream, or null while the stream holds no
  // whole message. A header of another side, or of a length that cannot
  // be, ends the room, since nothing after it can be read.
  #split(): ArrayBuffer | null {
    const pending = this.#pending;
    if (pending.length < HEADER_BYTES) return null;
    const header = pending.peek(HEADER_BYTES);
    const len = new DataView(header.buffer, header.byteOffset).getUint32(
      4,
      true,
    );
    if (
      !ENGINE_MAGIC.every((b, i) => header[i] === b) ||
      len % 8 !== 0 || len > MAX_MESSAGE_BYTES
    ) {
      this.#options.onError?.(new Error("not a stream of an engine"));
      this.end();
      return null;
    }
    if (pending.length < HEADER_BYTES + len) return null;
    pending.take(HEADER_BYTES);
    return pending.take(len).buffer as ArrayBuffer;
  }

  #read(payload: ArrayBuffer): void {
    const message = new $.Message(payload, false);
    if (this.#state === "hello") {
      const hello = message.getRoot(P.Hello);
      if (hello.minPlayers > 1 || hello.maxPlayers < 1) {
        this.#options.onError?.(
          new Error(`the game takes ${hello.minPlayers} players or more`),
        );
        this.end();
        return;
      }
      this.#start();
      return;
    }
    const m = message.getRoot(P.EngineToServer);
    switch (m.which()) {
      case P.EngineToServer.ASSET: {
        const out = new $.Message();
        const asset = out.initRoot(P.ServerToView)._initAsset();
        const blob = m.asset.blob.toUint8Array();
        asset.id = m.asset.id;
        asset._initBlob(blob.length).copyBuffer(blob);
        this.link.onmessage?.(new Uint8Array(out.toArrayBuffer()));
        break;
      }
      // The frame of player 1 and the frame of everyone are the same frame
      // in a room of one.
      case P.EngineToServer.FRAME: {
        const scene = m.frame.scene.toUint8Array();
        const out = new $.Message();
        out.initRoot(P.ServerToView)._initFrame(scene.length).copyBuffer(scene);
        this.link.onmessage?.(new Uint8Array(out.toArrayBuffer()));
        break;
      }
      case P.EngineToServer.TICK_TAKEN:
        this.#tickTaken = true;
        break;
      // An arm of a newer schema.
      default:
        break;
    }
  }

  #start(): void {
    const message = new $.Message();
    const start = message.initRoot(P.Start);
    start._initMembers(1).get(0).nickname = this.#options.nickname ?? "";
    start.tickRate = this.#options.tickRate ?? 60000;
    this.#write(message);
    this.#state = "open";
    this.link.onopen?.();
    this.#request();
  }

  // A ViewToServer of the view, which goes to the engine as the input of
  // player 1. A message with no event comes from a newer view.
  #input(payload: ArrayBuffer): void {
    if (this.#state !== "open") return;
    const event = new $.Message(payload, false).getRoot(P.ViewToServer);
    if (!event._hasEvent()) return;
    const message = new $.Message();
    const input = message.initRoot(P.ServerToEngine)._initInput();
    input.player = 1;
    input.event = event.event;
    this.#write(message);
  }

  // One tick an animation frame, and none while the engine has not taken
  // the last one, so a slow engine gets one tick and not a burst. A hidden
  // page has no animation frames and sends no ticks, and an engine that
  // keeps time goes on with its own clock.
  #tick(): void {
    this.#frame = 0;
    if (this.#state !== "open") return;
    if (this.#tickTaken) {
      this.#tickTaken = false;
      const message = new $.Message();
      message.initRoot(P.ServerToEngine)._initTick();
      this.#write(message);
    }
    this.#request();
  }

  #request(): void {
    if (this.#frame) return;
    const request = this.#options.requestFrame ??
      ((callback) => requestAnimationFrame(callback));
    this.#frame = request(() => this.#tick());
  }

  #stop(): void {
    this.#state = "ended";
    if (this.#frame) {
      const cancel = this.#options.cancelFrame ??
        ((handle) => cancelAnimationFrame(handle));
      cancel(this.#frame);
      this.#frame = 0;
    }
  }

  #write(message: $.Message): void {
    const payload = new Uint8Array(message.toArrayBuffer());
    const bytes = new Uint8Array(HEADER_BYTES + payload.length);
    bytes.set(SERVER_MAGIC);
    new DataView(bytes.buffer).setUint32(4, payload.length, true);
    bytes.set(payload, HEADER_BYTES);
    this.#options.write(bytes);
  }
}

// The bytes of a stream in the pieces that arrived. A message is copied
// out once it is whole, so an asset in many pieces costs one copy and not
// one per piece.
class Chunks {
  #chunks: Uint8Array[] = [];
  // Where the first chunk starts.
  #offset = 0;
  length = 0;

  // Keeps a copy of `bytes`, since the caller may reuse its buffer.
  push(bytes: Uint8Array): void {
    if (bytes.length === 0) return;
    this.#chunks.push(bytes.slice());
    this.length += bytes.length;
  }

  // The first `n` bytes, which stay.
  peek(n: number): Uint8Array {
    const first = this.#chunks[0];
    if (first && first.length - this.#offset >= n) {
      return first.subarray(this.#offset, this.#offset + n);
    }
    return this.#copy(n, false);
  }

  // The first `n` bytes in a buffer of their own, which capnp-es reads in
  // place.
  take(n: number): Uint8Array {
    return this.#copy(n, true);
  }

  #copy(n: number, consume: boolean): Uint8Array {
    const out = new Uint8Array(n);
    let filled = 0;
    let index = 0;
    let offset = this.#offset;
    while (filled < n) {
      const chunk = this.#chunks[index];
      const count = Math.min(n - filled, chunk.length - offset);
      out.set(chunk.subarray(offset, offset + count), filled);
      filled += count;
      offset += count;
      if (offset === chunk.length) {
        index++;
        offset = 0;
      }
    }
    if (consume) {
      this.#chunks.splice(0, index);
      this.#offset = offset;
      this.length -= n;
    }
    return out;
  }
}

// A room for an engine that runs in the same page, such as an engine
// compiled to WebAssembly in a worker, with every player at the same
// keyboard. The room plays the server: it reads the stream that the engine
// writes on fd 4, answers its hello with a start, ticks once an animation
// frame, and writes the input of each view to the stream that the engine
// reads on fd 3. Toward each view it is a Link that carries the messages of
// a server, so a View draws an engine of the page as it draws one behind a
// WebSocket.
//
// A view per player, all in one page, make most of the rules of a server
// moot: every asset goes to every view and fits, since the engine checks
// the limits of a room before it sends a frame, and no asset is ever lost.

import * as $ from "capnp-es";
import * as P from "./capnp/protocol.ts";
import type { Link } from "./view.ts";

export interface RoomOptions {
  // Writes the bytes of the stream that the engine reads, whole messages
  // only. The room never writes part of one.
  write(bytes: Uint8Array): void;
  // The nickname of each player in the start.
  nickname?: string;
  // How many players play, given the fewest and the most that the game
  // takes, from 1 to MAX_PLAYERS. The fewest when none is given, as a
  // display of the engine's own process does. A count outside the range
  // ends the room.
  players?: (min: number, max: number) => number;
  // The links of the players, from player 1, once the room knows how many
  // play and before any of them opens, so the host can attach a view to
  // each. The first is `link`.
  onStart?: (links: readonly Link[]) => void;
  // The rate of the ticks in the start, in thousandths of a hertz, 60000
  // when none is given. The room ticks at each animation frame, so the
  // rate is nominal, and an engine that keeps time measures it.
  tickRate?: number;
  // A stream of the engine that breaks the protocol. The room ends.
  onError?: (error: unknown) => void;
  // The room ended on its own, over a hello it refuses or a stream it
  // cannot read. The engine waits on for a start or a tick that will never
  // come, so the host tells it that the session is over.
  onGiveUp?: () => void;
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
// The most players at one keyboard, as MAX_LOCAL_PLAYERS of a stage in
// Rust.
export const MAX_PLAYERS = 8;

export class Room {
  #options: RoomOptions;
  // The bytes of the engine that make no whole message yet.
  #pending = new Chunks();
  #state: State = "hello";
  // Whether the engine took the last tick, so the next one can go.
  #tickTaken = true;
  #frame = 0;
  // The link of each player, from player 1. The others join at the start.
  #links: Link[];
  // The link of player 1, there from the construction, so a host of one
  // player attaches its view before the hello.
  readonly link: Link;

  constructor(options: RoomOptions) {
    this.#options = options;
    this.link = this.#link(1);
    this.#links = [this.link];
  }

  #link(player: number): Link {
    return {
      send: (payload) => this.#input(player, payload),
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
    for (const link of this.#links) link.onclose?.(1000, "");
  }

  #giveUp(): void {
    this.end();
    this.#options.onGiveUp?.();
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
      this.#giveUp();
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
      const players = this.#players(hello.minPlayers, hello.maxPlayers);
      if (players === null) {
        this.#giveUp();
        return;
      }
      this.#start(players);
      return;
    }
    const m = message.getRoot(P.EngineToServer);
    switch (m.which()) {
      // Every view gets every asset, since a frame to one player may draw
      // it next.
      case P.EngineToServer.ASSET: {
        const out = new $.Message();
        const asset = out.initRoot(P.ServerToView)._initAsset();
        const blob = m.asset.blob.toUint8Array();
        asset.id = m.asset.id;
        asset._initBlob(blob.length).copyBuffer(blob);
        this.#send(this.#links, out);
        break;
      }
      // Player 0 is every player. A player that does not play drops the
      // frame.
      case P.EngineToServer.FRAME: {
        const scene = m.frame.scene.toUint8Array();
        const out = new $.Message();
        out.initRoot(P.ServerToView)._initFrame(scene.length).copyBuffer(scene);
        const player = m.frame.player;
        const to = player === 0
          ? this.#links
          : this.#links.slice(player - 1, player);
        this.#send(to, out);
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

  // How many play a game of `min` to `max` players, or null when the room
  // cannot hold them.
  #players(min: number, max: number): number | null {
    const fail = (message: string) => {
      this.#options.onError?.(new Error(message));
      return null;
    };
    if (min < 1 || max < min) {
      return fail(`a game of ${min} to ${max} players`);
    }
    if (min > MAX_PLAYERS) {
      return fail(
        `the game takes ${min} players, and at most ${MAX_PLAYERS} play here`,
      );
    }
    const players = this.#options.players?.(min, Math.min(max, MAX_PLAYERS)) ??
      min;
    if (
      !Number.isInteger(players) || players < min || players > max ||
      players > MAX_PLAYERS
    ) {
      return fail(
        `the game takes from ${min} to ${max} players, and ${players} play here`,
      );
    }
    return players;
  }

  #start(players: number): void {
    for (let player = 2; player <= players; player++) {
      this.#links.push(this.#link(player));
    }
    this.#options.onStart?.(this.#links);
    const message = new $.Message();
    const start = message.initRoot(P.Start);
    const members = start._initMembers(players);
    for (let i = 0; i < players; i++) {
      members.get(i).nickname = this.#options.nickname ?? "";
    }
    start.tickRate = this.#options.tickRate ?? 60000;
    this.#write(message);
    this.#state = "open";
    for (const link of this.#links) link.onopen?.();
    this.#request();
  }

  #send(links: readonly Link[], message: $.Message): void {
    const payload = new Uint8Array(message.toArrayBuffer());
    for (const link of links) link.onmessage?.(payload);
  }

  // A ViewToServer of the view of `player`, which goes to the engine as the
  // input of that player. A message with no event comes from a newer view.
  #input(player: number, payload: ArrayBuffer): void {
    if (this.#state !== "open") return;
    const event = new $.Message(payload, false).getRoot(P.ViewToServer);
    if (!event._hasEvent()) return;
    const message = new $.Message();
    const input = message.initRoot(P.ServerToEngine)._initInput();
    input.player = player;
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

import * as $ from "capnp-es";
import { assertEquals } from "@std/assert";
import * as E from "../src/capnp/event.ts";
import * as P from "../src/capnp/protocol.ts";
import { encodeInput } from "../src/protocol.ts";
import { MAX_PLAYERS, Room } from "../src/room.ts";
import type { Link } from "../src/view.ts";

// What an engine writes: the envelope of framing.rs around a message.
function engine(message: $.Message): Uint8Array {
  const payload = new Uint8Array(message.toArrayBuffer());
  const bytes = new Uint8Array(8 + payload.length);
  bytes.set([0x53, 0x49, 0x45, 0x31]);
  new DataView(bytes.buffer).setUint32(4, payload.length, true);
  bytes.set(payload, 8);
  return bytes;
}

function hello(min: number, max: number): Uint8Array {
  const m = new $.Message();
  const h = m.initRoot(P.Hello);
  h.minPlayers = min;
  h.maxPlayers = max;
  return engine(m);
}

function tickTaken(): Uint8Array {
  const m = new $.Message();
  m.initRoot(P.EngineToServer).tickTaken = true;
  return engine(m);
}

function frame(scene: Uint8Array, player = 0): Uint8Array {
  const m = new $.Message();
  const f = m.initRoot(P.EngineToServer)._initFrame();
  f.player = player;
  f._initScene(scene.length).copyBuffer(scene);
  return engine(m);
}

// The payloads of the server, with the envelope checked and taken off.
function payloads(written: Uint8Array[]): ArrayBuffer[] {
  return written.map((bytes) => {
    assertEquals([...bytes.subarray(0, 4)], [0x53, 0x49, 0x53, 0x31]);
    const len = new DataView(bytes.buffer, bytes.byteOffset).getUint32(4, true);
    assertEquals(bytes.length, 8 + len);
    return bytes.slice(8).buffer;
  });
}

function setup() {
  const written: Uint8Array[] = [];
  const frames: ((now: number) => void)[] = [];
  const room = new Room({
    write: (bytes) => written.push(bytes),
    nickname: "ana",
    requestFrame: (callback) => frames.push(callback),
    cancelFrame: () => {},
  });
  const next = (now: number) => frames.shift()!(now);
  return { room, written, next };
}

Deno.test("the hello of a game of one is answered with a start", () => {
  const { room, written } = setup();
  let opened = false;
  room.link.onopen = () => opened = true;
  room.receive(hello(1, 4));
  const [start] = payloads(written);
  const s = new $.Message(start, false).getRoot(P.Start);
  assertEquals([s.members.length, s.members.get(0).nickname], [1, "ana"]);
  assertEquals(s.tickRate, 60000);
  assertEquals(opened, true);
});

Deno.test("a game of two players or more starts with the fewest", () => {
  const written: Uint8Array[] = [];
  let links: readonly Link[] = [];
  let opened = 0;
  const room = new Room({
    write: (bytes) => written.push(bytes),
    onStart: (all) => {
      links = all;
      for (const link of all) link.onopen = () => opened++;
    },
  });
  room.receive(hello(2, 4));
  const s = new $.Message(payloads(written)[0], false).getRoot(P.Start);
  assertEquals([s.members.length, links.length, opened], [2, 2, 2]);
  assertEquals(links[0], room.link);
});

Deno.test("the host picks the players inside the range of the game", () => {
  const written: Uint8Array[] = [];
  let range: number[] = [];
  const room = new Room({
    write: (bytes) => written.push(bytes),
    players: (min, max) => {
      range = [min, max];
      return 3;
    },
  });
  room.receive(hello(1, 20));
  const s = new $.Message(payloads(written)[0], false).getRoot(P.Start);
  assertEquals([range, s.members.length], [[1, MAX_PLAYERS], 3]);
});

Deno.test("players that the room cannot hold end the room", () => {
  const cases: [number, number, number | undefined][] = [
    [1, 2, 3],
    [9, 10, undefined],
    [2, 2, 1],
  ];
  for (const [min, max, players] of cases) {
    let gaveUp = false;
    const errors: unknown[] = [];
    const written: Uint8Array[] = [];
    const room = new Room({
      write: (bytes) => written.push(bytes),
      players: players === undefined ? undefined : () => players,
      onError: (e) => errors.push(e),
      onGiveUp: () => gaveUp = true,
    });
    let closed = false;
    room.link.onclose = () => closed = true;
    room.receive(hello(min, max));
    assertEquals([written.length, closed, gaveUp, errors.length], [
      0,
      true,
      true,
      1,
    ]);
  }
});

Deno.test("a frame goes to its player, and player 0 to all", () => {
  const got: number[][] = [[], []];
  const room = new Room({
    write: () => {},
    players: () => 2,
    onStart: (all) =>
      all.forEach((link, i) =>
        link.onmessage = (payload) =>
          got[i].push(
            new $.Message(payload, false).getRoot(P.ServerToView).frame
              .toUint8Array()[0],
          )
      ),
  });
  room.receive(hello(1, 2));
  room.receive(frame(new Uint8Array(8).fill(1), 2));
  room.receive(frame(new Uint8Array(8).fill(2), 0));
  room.receive(frame(new Uint8Array(8).fill(3), 1));
  room.receive(frame(new Uint8Array(8).fill(4), 3));
  assertEquals(got, [[2, 3], [1, 2]]);
});

Deno.test("the input of a view goes to the engine as its player", () => {
  let links: readonly Link[] = [];
  const written: Uint8Array[] = [];
  const room = new Room({
    write: (bytes) => written.push(bytes),
    players: () => 2,
    onStart: (all) => links = all,
  });
  room.receive(hello(1, 2));
  links[1].send(encodeInput({ kind: "resize", width: 320, height: 200 }));
  const input = new $.Message(payloads(written)[1], false)
    .getRoot(P.ServerToEngine).input;
  assertEquals(input.player, 2);
});

Deno.test("a room that the host ends does not give up", () => {
  let gaveUp = false;
  const room = new Room({ write: () => {}, onGiveUp: () => gaveUp = true });
  room.receive(hello(1, 1));
  room.end();
  assertEquals(gaveUp, false);
});

Deno.test("a message in pieces is read once it is whole", () => {
  const { room, written } = setup();
  const bytes = hello(1, 1);
  room.receive(bytes.subarray(0, 5));
  room.receive(bytes.subarray(5, 11));
  assertEquals(written.length, 0);
  room.receive(bytes.subarray(11));
  assertEquals(written.length, 1);
});

Deno.test("the next tick waits for the engine to take the last one", () => {
  const { room, written, next } = setup();
  room.receive(hello(1, 1));
  next(0);
  next(17);
  next(34);
  assertEquals(written.length, 2);
  room.receive(tickTaken());
  next(51);
  const ticks = payloads(written).slice(1).map((p) =>
    new $.Message(p, false).getRoot(P.ServerToEngine).which()
  );
  assertEquals(ticks, [P.ServerToEngine.TICK, P.ServerToEngine.TICK]);
});

Deno.test("an engine that keeps up gets a tick at every frame", () => {
  const { room, written, next } = setup();
  room.receive(hello(1, 1));
  for (let i = 0; i < 12; i++) {
    next(i * 1000 / 120);
    room.receive(tickTaken());
  }
  assertEquals(written.length - 1, 12);
});

Deno.test("messages in pieces of any size come out whole and in order", () => {
  const { room } = setup();
  const got: Uint8Array[] = [];
  room.link.onmessage = (payload) => got.push(payload);
  const scenes = [new Uint8Array(24).fill(1), new Uint8Array(40).fill(2)];
  const stream = [hello(1, 1), ...scenes.map(frame)];
  const bytes = new Uint8Array(stream.reduce((n, b) => n + b.length, 0));
  let at = 0;
  for (const b of stream) {
    bytes.set(b, at);
    at += b.length;
  }
  for (let i = 0; i < bytes.length; i += 3) {
    room.receive(bytes.subarray(i, i + 3));
  }
  const frames = got.map((p) =>
    new $.Message(p.buffer as ArrayBuffer, false).getRoot(P.ServerToView)
      .frame.toUint8Array()
  );
  assertEquals(frames, scenes);
});

Deno.test("a frame goes to the view as a frame of a server", () => {
  const { room } = setup();
  const got: Uint8Array[] = [];
  room.link.onmessage = (payload) => got.push(payload);
  room.receive(hello(1, 1));
  room.receive(frame(new Uint8Array([1, 2, 3, 4, 5, 6, 7, 8])));
  const m = new $.Message(got[0], false).getRoot(P.ServerToView);
  assertEquals(m.which(), P.ServerToView.FRAME);
  assertEquals([...m.frame.toUint8Array()], [1, 2, 3, 4, 5, 6, 7, 8]);
});

Deno.test("the input of the view goes to the engine as player 1", () => {
  const { room, written } = setup();
  room.receive(hello(1, 1));
  room.link.send(encodeInput({ kind: "resize", width: 320, height: 200 }));
  const input = new $.Message(payloads(written)[1], false)
    .getRoot(P.ServerToEngine).input;
  assertEquals(input.player, 1);
  assertEquals(input.event.which(), E.InputEvent.RESIZE);
  assertEquals([input.event.resize.width, input.event.resize.height], [
    320,
    200,
  ]);
});

Deno.test("a stream of another side ends the room", () => {
  const errors: unknown[] = [];
  let gaveUp = false;
  const room = new Room({
    write: () => {},
    onError: (e) => errors.push(e),
    onGiveUp: () => gaveUp = true,
  });
  let closed = false;
  room.link.onclose = () => closed = true;
  const bytes = hello(1, 1);
  bytes[2] = 0x53; // SIS1, the magic of a server
  room.receive(bytes);
  assertEquals([errors.length, closed, gaveUp], [1, true, true]);
});

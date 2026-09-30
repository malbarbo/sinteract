import * as $ from "capnp-es";
import { assertEquals, assertThrows } from "@std/assert";
import * as W from "../src/capnp/scene.ts";
import { decodeScene, type Element, MAX_NESTING } from "../src/scene.ts";

// A scene of 100 by 50 whose elements `fill` writes, as bytes.
function sceneBytes(
  n: number,
  fill: (list: $.List<W.Element>) => void,
): Uint8Array {
  const message = new $.Message();
  const scene = message.initRoot(W.Scene);
  scene.width = 100;
  scene.height = 50;
  fill(scene._initElements(n));
  return new Uint8Array(message.toArrayBuffer());
}

function decode(bytes: Uint8Array, images: number[] = []): Element[] {
  return decodeScene(bytes, (id) => images.includes(id)).elements;
}

// A path of a red fill with `verbs` and `coords`.
function writePath(
  e: W.Element,
  verbs: number[],
  coords: number[],
): W.PathStyle {
  const p = e._initPath();
  const style = p._initStyle();
  const c = style._initFill()._initSolid();
  c.r = 255;
  c.a = 1;
  p._initVerbs(verbs.length).copyBuffer(new Uint8Array(verbs));
  const list = p._initCoords(coords.length);
  coords.forEach((v, i) => list.set(i, v));
  return style;
}

Deno.test("a path begins at the origin, merges moves and drops the last move", () => {
  const bytes = sceneBytes(
    1,
    (l) => writePath(l.get(0), [1, 0, 0, 1, 0], [5, 6, 1, 1, 2, 2, 3, 3, 9, 9]),
  );
  const [path] = decode(bytes);
  if (path.kind !== "path") throw new Error(path.kind);
  assertEquals([...path.segments.verbs], [0, 1, 0, 1]);
  assertEquals([...path.segments.coords], [0, 0, 5, 6, 2, 2, 3, 3]);
});

Deno.test("coords that do not match the verbs make the scene unusable", () => {
  const bytes = sceneBytes(1, (l) => writePath(l.get(0), [1, 1], [1, 2, 3]));
  assertThrows(() => decode(bytes));
});

Deno.test("an unknown verb, enum value or float that is not finite skips the element", () => {
  const bytes = sceneBytes(4, (l) => {
    writePath(l.get(0), [7], [1, 2]);
    writePath(l.get(1), [1], [1, 2]).lineCap = 9 as W.LineCap;
    writePath(l.get(2), [1], [NaN, 2]);
    writePath(l.get(3), [1], [1, 2]);
  });
  assertEquals(decode(bytes).length, 1);
});

Deno.test("an element of an unknown arm is skipped", () => {
  const bytes = sceneBytes(2, (l) => {
    writePath(l.get(0), [1], [1, 2]);
    $.utils.setUint16(0, 99, l.get(0));
    writePath(l.get(1), [1], [1, 2]);
  });
  assertEquals(decode(bytes).length, 1);
});

Deno.test("a paint of an unknown arm draws its fallback, or skips the element", () => {
  const bytes = sceneBytes(2, (l) => {
    for (const i of [0, 1]) {
      const fill = writePath(l.get(i), [1], [1, 2]).fill;
      $.utils.setUint16(0, 9, fill);
      fill.fallback = 0x11223380;
      fill.hasFallback = i === 0;
    }
  });
  const elements = decode(bytes);
  assertEquals(elements.length, 1);
  const [path] = elements;
  if (path.kind !== "path") throw new Error(path.kind);
  assertEquals(path.style.fill, {
    kind: "solid",
    color: { r: 0x11, g: 0x22, b: 0x33, a: 0x80 / 255 },
  });
});

Deno.test("the stops rise and stay in [0, 1], and a gradient with no extent is solid", () => {
  const bytes = sceneBytes(2, (l) => {
    for (const [i, x1] of [[0, 10], [1, 0]]) {
      const fill = writePath(l.get(i), [1], [1, 2])._initFill();
      const g = fill._initLinear();
      g.x1 = x1;
      const stops = g._initStops(3);
      [0.5, 0.2, 1.5].forEach((offset, k) => {
        stops.get(k).offset = offset;
        stops.get(k)._initColor().b = 10 * k;
      });
    }
  });
  const [a, b] = decode(bytes);
  if (a.kind !== "path" || a.style.fill.kind !== "linear") throw new Error();
  assertEquals(a.style.fill.stops.map((s) => s.offset), [0.5, 0.5, 1]);
  if (b.kind !== "path") throw new Error();
  assertEquals(b.style.fill, {
    kind: "solid",
    color: { r: 0, g: 0, b: 20, a: 0 },
  });
});

Deno.test("an odd dash repeats, and a dash that sums to zero is solid", () => {
  const bytes = sceneBytes(2, (l) => {
    for (const [i, dash] of [[0, [4, 2, 1]], [1, [0, 0]]] as const) {
      const style = writePath(l.get(i), [1], [1, 2]);
      const list = style._initDashArray(dash.length);
      dash.forEach((v, k) => list.set(k, v));
    }
  });
  const [a, b] = decode(bytes);
  if (a.kind !== "path" || b.kind !== "path") throw new Error();
  assertEquals(a.style.dash, { array: [4, 2, 1, 4, 2, 1], offset: 0 });
  assertEquals(b.style.dash, null);
});

Deno.test("a layer of opacity 1 or more is its elements, and one of 0 or less draws nothing", () => {
  const bytes = sceneBytes(3, (l) => {
    [1.5, 0.5, 0].forEach((opacity, i) => {
      const layer = l.get(i)._initLayer();
      layer.opacity = opacity;
      writePath(layer._initElements(1).get(0), [1], [1, 2]);
    });
  });
  assertEquals(decode(bytes).map((e) => e.kind), ["path", "layer"]);
});

Deno.test("a clip or a layer past MAX_NESTING draws nothing", () => {
  const bytes = sceneBytes(1, (l) => {
    let e = l.get(0);
    for (let depth = 0; depth <= MAX_NESTING; depth++) {
      const layer = e._initLayer();
      layer.opacity = 0.5;
      const inner = layer._initElements(2);
      writePath(inner.get(1), [1], [1, 2]);
      e = inner.get(0);
    }
  });
  let elements = decode(bytes);
  let depth = 0;
  while (elements[0]?.kind === "layer") {
    elements = elements[0].elements;
    depth++;
  }
  assertEquals(depth, MAX_NESTING);
  assertEquals(elements.map((e) => e.kind), ["path"]);
});

Deno.test("a clip that holds a float that is not finite drops all it holds", () => {
  const bytes = sceneBytes(1, (l) => {
    const c = l.get(0)._initClipped();
    const clip = c._initClip();
    clip._initVerbs(1).copyBuffer(new Uint8Array([1]));
    const coords = clip._initCoords(2);
    coords.set(0, Infinity);
    writePath(c._initElements(1).get(0), [1], [1, 2]);
  });
  assertEquals(decode(bytes), []);
});

Deno.test("a bitmap with no image is skipped", () => {
  const bytes = sceneBytes(2, (l) => {
    for (const i of [0, 1]) {
      const b = l.get(i)._initBitmap();
      b.id = i;
      b.m0 = 10;
      b.m3 = 10;
    }
  });
  assertEquals(decode(bytes, [1]).map((e) => e.kind === "bitmap" && e.id), [1]);
});

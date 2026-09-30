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

function decode(bytes: Uint8Array): Element[] {
  return decodeScene(bytes).elements;
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
  c.a = 255;
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
    color: { r: 0x11, g: 0x22, b: 0x33, a: 0x80 },
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

Deno.test("a gradient with a float that is not finite skips its element, unless it has no stops", () => {
  const bytes = sceneBytes(3, (l) => {
    const radial = writePath(l.get(0), [1], [1, 2])._initFill()._initRadial();
    radial.radius = -Infinity;
    radial._initStops(1);
    const linear = writePath(l.get(1), [1], [1, 2])._initFill()._initLinear();
    linear.x1 = NaN;
    linear._initStops(1).get(0).offset = NaN;
    const empty = writePath(l.get(2), [1], [1, 2])._initFill()._initLinear();
    empty.x1 = NaN;
  });
  assertEquals(
    decode(bytes).map((e) => e.kind === "path" && e.style.fill),
    [{ kind: "solid", color: { r: 0, g: 0, b: 0, a: 0 } }],
  );
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

Deno.test("each field of the schema reads from its place in the layout", () => {
  const bytes = sceneBytes(4, (l) => {
    const style = writePath(l.get(0), [1], [8, 9]);
    const linear = style._initFill()._initLinear();
    [linear.x0, linear.y0, linear.x1, linear.y1] = [1, 2, 30, 4];
    linear.spread = W.SpreadMode.REFLECT;
    const stops = linear._initStops(2);
    stops.get(0).offset = 0.25;
    stops.get(0)._initColor().r = 10;
    stops.get(1).offset = 0.75;
    const last = stops.get(1)._initColor();
    [last.g, last.b, last.a] = [20, 30, 128];
    const radial = style._initStroke()._initRadial();
    [radial.cx, radial.cy, radial.radius] = [5, 6, 7];
    radial.spread = W.SpreadMode.REPEAT;
    radial._initStops(1).get(0)._initColor().a = 255;
    style.strokeWidth = 3;
    style.lineCap = W.LineCap.ROUND;
    style.lineJoin = W.LineJoin.BEVEL;
    style.fillRule = W.FillRule.EVEN_ODD;
    style.closed = true;
    style.miterLimit = 6;
    const dash = style._initDashArray(2);
    dash.set(0, 4);
    dash.set(1, 2);
    style.dashOffset = 1.5;

    const clipped = l.get(1)._initClipped();
    const clip = clipped._initClip();
    clip.fillRule = W.FillRule.EVEN_ODD;
    clip._initVerbs(1).copyBuffer(new Uint8Array([1]));
    const coords = clip._initCoords(2);
    coords.set(0, 1);
    coords.set(1, 2);
    const bitmap = clipped._initElements(1).get(0)._initBitmap();
    bitmap.id = 42;
    [bitmap.m0, bitmap.m1, bitmap.m2] = [1, 2, 3];
    [bitmap.m3, bitmap.m4, bitmap.m5] = [4, 5, 6];
    bitmap.sampling = W.Sampling.NEAREST;

    const text = l.get(2)._initText();
    const fill = text._initFill()._initSolid();
    [fill.r, fill.g, fill.b, fill.a] = [1, 2, 3, 128];
    const stroke = text._initStroke()._initSolid();
    [stroke.r, stroke.g, stroke.b, stroke.a] = [4, 5, 6, 255];
    text.strokeWidth = 2;
    [text.m0, text.m1, text.m2] = [7, 8, 9];
    [text.m3, text.m4, text.m5] = [10, 11, 12];
    text.size = 12;
    text.family = "serif";
    text.weight = 700;
    text.style = W.FontStyle.ITALIC;
    text.underline = true;
    text.text = "Olá";

    const layer = l.get(3)._initLayer();
    layer.opacity = 0.25;
    layer._initElements(1).get(0)._initBitmap().id = 7;
  });
  const transparent = { r: 0, g: 0, b: 0, a: 0 };
  assertEquals(JSON.parse(JSON.stringify(decode(bytes), typedArrays)), [
    {
      kind: "path",
      style: {
        fill: {
          kind: "linear",
          x0: 1,
          y0: 2,
          x1: 30,
          y1: 4,
          stops: [
            { offset: 0.25, color: { r: 10, g: 0, b: 0, a: 0 } },
            { offset: 0.75, color: { r: 0, g: 20, b: 30, a: 128 } },
          ],
          spread: "reflect",
        },
        stroke: {
          kind: "radial",
          cx: 5,
          cy: 6,
          radius: 7,
          stops: [{ offset: 0, color: { ...transparent, a: 255 } }],
          spread: "repeat",
        },
        strokeWidth: 3,
        lineCap: "round",
        lineJoin: "bevel",
        fillRule: "evenodd",
        closed: true,
        miterLimit: 6,
        dash: { array: [4, 2], offset: 1.5 },
      },
      segments: { verbs: [0, 1], coords: [0, 0, 8, 9] },
    },
    {
      kind: "clipped",
      clip: { verbs: [0, 1], coords: [0, 0, 1, 2] },
      fillRule: "evenodd",
      elements: [{
        kind: "bitmap",
        id: 42,
        transform: [1, 2, 3, 4, 5, 6],
        sampling: "nearest",
      }],
    },
    {
      kind: "text",
      fill: { kind: "solid", color: { r: 1, g: 2, b: 3, a: 128 } },
      stroke: { kind: "solid", color: { r: 4, g: 5, b: 6, a: 255 } },
      strokeWidth: 2,
      transform: [7, 8, 9, 10, 11, 12],
      size: 12,
      family: "serif",
      weight: 700,
      style: "italic",
      underline: true,
      text: "Olá",
    },
    {
      kind: "layer",
      opacity: 0.25,
      elements: [{
        kind: "bitmap",
        id: 7,
        transform: [0, 0, 0, 0, 0, 0],
        sampling: "smooth",
      }],
    },
  ]);
});

// Writes a typed array as a plain array, for JSON.
function typedArrays(_: string, v: unknown): unknown {
  return ArrayBuffer.isView(v) ? [...(v as Float32Array)] : v;
}

Deno.test("a root behind a far pointer reads through its landing pad", () => {
  const words = new BigUint64Array([
    // Two segments of 1 and 2 words.
    1n | (1n << 32n),
    2n,
    // Segment 0, the root: a far pointer to word 0 of segment 1.
    2n | (1n << 32n),
    // Segment 1, the pad: a pointer to the next word, a struct of one data
    // word, the Scene with a width of 100 and a height of 50.
    1n << 32n,
    0x42c80000n | (0x42480000n << 32n),
  ]);
  const scene = decodeScene(new Uint8Array(words.buffer));
  assertEquals([scene.width, scene.height], [100, 50]);
});

Deno.test("a root behind a double far pointer reads through its landing pad", () => {
  const words = new BigUint64Array([
    // Three segments of 1, 2 and 1 words.
    2n | (1n << 32n),
    2n | (1n << 32n),
    // Segment 0, the root: a double far pointer to word 0 of segment 1.
    6n | (1n << 32n),
    // Segment 1, the pad: a far pointer to word 0 of segment 2, and a tag
    // of a struct of one data word.
    2n | (2n << 32n),
    1n << 32n,
    // Segment 2, the Scene: a width of 100 and a height of 50.
    0x42c80000n | (0x42480000n << 32n),
  ]);
  const scene = decodeScene(new Uint8Array(words.buffer));
  assertEquals([scene.width, scene.height], [100, 50]);
});

Deno.test("a truncated message, a pointer out of its segment or a Text that is not UTF-8 throws", () => {
  const bytes = sceneBytes(1, (l) => {
    writePath(l.get(0), [1], [1, 2]);
  });
  assertThrows(() => decodeScene(bytes.subarray(0, bytes.length - 8)));
  const far = bytes.slice();
  new DataView(far.buffer).setUint32(8, 0x7ffffc, true);
  assertThrows(() => decodeScene(far));
  const text = sceneBytes(1, (l) => {
    l.get(0)._initText().text = "zzzz";
  });
  text[text.indexOf("z".charCodeAt(0))] = 0xff;
  assertThrows(() => decodeScene(text));
});

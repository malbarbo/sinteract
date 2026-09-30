// The scene of a frame, as plain values, and its decoder. The decoder reads
// the bytes of a message whose root is the Scene of schema/scene.capnp, with
// the rules of the Rust reader in src/wire/scene.rs and of the builders of
// src/scene.rs. It skips an element of an arm it does not know, one that
// holds a value it does not know and one that holds a float that is not
// finite. Damage, such as a bad pointer or verbs that disagree with their
// coords, throws, and the whole scene is unusable.

import * as $ from "capnp-es";
import * as W from "./capnp/scene.ts";

export interface Scene {
  width: number;
  height: number;
  elements: Element[];
}

export type Element =
  | PathElement
  | Clipped
  | TextElement
  | BitmapElement
  | Layer;

export interface PathElement {
  kind: "path";
  style: PathStyle;
  segments: Segments;
}

// The clip closes each sub-path, and a clip with no segments hides all that
// it holds.
export interface Clipped {
  kind: "clipped";
  clip: Segments;
  fillRule: CanvasFillRule;
  elements: Element[];
}

// A glyph draws in text space, with `size` units to the em and the origin at
// the center of a box that spans the advance of the text and the ascender to
// the descender of the face. `transform` maps text space to the scene.
export interface TextElement {
  kind: "text";
  fill: Paint;
  stroke: Paint;
  strokeWidth: number;
  transform: Transform;
  size: number;
  family: string;
  weight: number;
  style: "normal" | "italic" | "oblique";
  underline: boolean;
  text: string;
}

// The image fills the unit square (-0.5..0.5, -0.5..0.5), and `transform`
// maps the square to the scene.
export interface BitmapElement {
  kind: "bitmap";
  id: number;
  transform: Transform;
  sampling: "smooth" | "nearest";
}

// The elements draw into a transparent layer, which draws with `opacity`, in
// (0, 1).
export interface Layer {
  kind: "layer";
  opacity: number;
  elements: Element[];
}

// x' = a * x + c * y + e, y' = b * x + d * y + f, as in setTransform.
export type Transform = [number, number, number, number, number, number];

export interface PathStyle {
  fill: Paint;
  stroke: Paint;
  strokeWidth: number;
  lineCap: CanvasLineCap;
  lineJoin: CanvasLineJoin;
  fillRule: CanvasFillRule;
  // Closes every sub-path, so the stroke joins at the start of each one.
  closed: boolean;
  // At least 1.
  miterLimit: number;
  // `null` is a solid stroke.
  dash: Dash | null;
}

// An even number of lengths, none negative, with a positive sum.
export interface Dash {
  array: number[];
  offset: number;
}

export interface Rgba {
  r: number;
  g: number;
  b: number;
  a: number;
}

// A gradient has at least one stop and has extent. The offsets rise and
// stay in [0, 1].
export type Paint =
  | { kind: "solid"; color: Rgba }
  | {
    kind: "linear";
    x0: number;
    y0: number;
    x1: number;
    y1: number;
    stops: Stop[];
    spread: Spread;
  }
  | {
    kind: "radial";
    cx: number;
    cy: number;
    radius: number;
    stops: Stop[];
    spread: Spread;
  };

export interface Stop {
  offset: number;
  color: Rgba;
}

export type Spread = "pad" | "reflect" | "repeat";

// One byte per segment and the coords that the verbs consume, in order. The
// first verb is a move, a move never follows a move, and the last verb is
// not a move.
export interface Segments {
  verbs: Uint8Array;
  coords: Float32Array;
}

// The verb bytes of scene.capnp. capnp-es generates no constants.
export const VERB_MOVE = 0;
export const VERB_LINE = 1;
export const VERB_QUAD = 2;
export const VERB_CUBIC = 3;

// The most clips and layers, one inside another, as MAX_NESTING in Rust.
export const MAX_NESTING = 16;

// Returns `true` if the paint marks at least one pixel, `false` otherwise.
export function isVisible(paint: Paint): boolean {
  return paint.kind !== "solid" || paint.color.a > 0;
}

// Reads `bytes`, a whole message whose root is a Scene. `hasImage` says if a
// bitmap of an id has an image, and a bitmap without one is skipped.
export function decodeScene(
  bytes: Uint8Array,
  hasImage: (id: number) => boolean,
): Scene {
  const root = new $.Message(bytes, false).getRoot(W.Scene);
  return {
    width: frameSize(root.width),
    height: frameSize(root.height),
    elements: readElements(root.elements, 0, hasImage),
  };
}

// Why the decoder skips an element: a value from a newer schema, a float
// that is not finite or a bitmap with no image. Any other error is damage.
class Skip extends Error {}

function frameSize(size: number): number {
  return Number.isFinite(size) && size > 0 ? size : 0;
}

function readElements(
  list: $.List<W.Element>,
  depth: number,
  hasImage: (id: number) => boolean,
): Element[] {
  const out: Element[] = [];
  for (let i = 0; i < list.length; i++) {
    try {
      readElement(list.get(i), depth, hasImage, out);
    } catch (e) {
      if (!(e instanceof Skip)) throw e;
    }
  }
  return out;
}

function readElement(
  e: W.Element,
  depth: number,
  hasImage: (id: number) => boolean,
  out: Element[],
): void {
  switch (e.which()) {
    case W.Element.PATH: {
      const p = e.path;
      const style = readPathStyle(p.style);
      out.push({
        kind: "path",
        style,
        segments: readSegments(p.verbs, p.coords),
      });
      return;
    }
    case W.Element.CLIPPED: {
      if (depth >= MAX_NESTING) return;
      const c = e.clipped;
      const clip = c.clip;
      const fillRule = fillRuleOf(clip.fillRule);
      const segments = readSegments(clip.verbs, clip.coords);
      const elements = readElements(c.elements, depth + 1, hasImage);
      out.push({ kind: "clipped", clip: segments, fillRule, elements });
      return;
    }
    case W.Element.TEXT:
      out.push(readText(e.text));
      return;
    case W.Element.BITMAP: {
      const b = e.bitmap;
      if (!hasImage(b.id)) throw new Skip();
      const sampling = b.sampling === W.Sampling.NEAREST ? "nearest" : "smooth";
      const transform = transformOf(b);
      out.push({ kind: "bitmap", id: b.id, transform, sampling });
      return;
    }
    case W.Element.LAYER: {
      if (depth >= MAX_NESTING) return;
      const l = e.layer;
      const elements = readElements(l.elements, depth + 1, hasImage);
      const opacity = l.opacity;
      if (!Number.isFinite(opacity) || opacity <= 0) return;
      if (opacity >= 1) out.push(...elements);
      else out.push({ kind: "layer", opacity, elements });
      return;
    }
    default:
      // An arm from a newer schema.
      return;
  }
}

function readPathStyle(s: W.PathStyle): PathStyle {
  return {
    fill: readPaint(s.fill),
    stroke: readPaint(s.stroke),
    strokeWidth: finiteNumber(s.strokeWidth),
    lineCap: lineCapOf(s.lineCap),
    lineJoin: lineJoinOf(s.lineJoin),
    fillRule: fillRuleOf(s.fillRule),
    closed: s.closed,
    miterLimit: Math.max(finiteNumber(s.miterLimit), 1),
    dash: readDash(s.dashArray.toArray(), s.dashOffset),
  };
}

// An odd array repeats to an even one. An array that draws a solid stroke,
// empty, with a negative length or with lengths that sum to zero, is `null`,
// as is one with a length or an offset that is not finite.
function readDash(array: number[], offset: number): Dash | null {
  if (array.length % 2 === 1) array = [...array, ...array];
  const sum = array.reduce((a, b) => a + b, 0);
  const valid = array.every((v) => v >= 0) && sum > 0 && Number.isFinite(sum);
  return valid && Number.isFinite(offset) ? { array, offset } : null;
}

function readText(t: W.TextNode): TextElement {
  return {
    kind: "text",
    fill: readPaint(t.fill),
    stroke: readPaint(t.stroke),
    strokeWidth: finiteNumber(t.strokeWidth),
    transform: transformOf(t),
    size: finiteNumber(t.size),
    family: t.family,
    weight: t.weight,
    style: fontStyleOf(t.style),
    underline: t.underline,
    text: t.text,
  };
}

// A gradient with no extent, a radius or an axis of 2^-15 or less, paints
// the color of its last stop, as in SVG. No stops paint transparent.
const NO_EXTENT = 1 / (1 << 15);

// The paint of `p`. As in Rust, a gradient collapses to a solid color before
// the check for a float that is not finite, so a gradient with no extent and
// a radius of -Infinity still draws.
function readPaint(p: W.Paint): Paint {
  let paint: Paint;
  switch (p.which()) {
    case W.Paint.SOLID:
      paint = { kind: "solid", color: readRgba(p.solid) };
      break;
    case W.Paint.LINEAR: {
      const g = p.linear;
      paint = collapse({
        kind: "linear",
        x0: g.x0,
        y0: g.y0,
        x1: g.x1,
        y1: g.y1,
        stops: readStops(g.stops),
        spread: spreadOf(g.spread),
      });
      break;
    }
    case W.Paint.RADIAL: {
      const g = p.radial;
      paint = collapse({
        kind: "radial",
        cx: g.cx,
        cy: g.cy,
        radius: g.radius,
        stops: readStops(g.stops),
        spread: spreadOf(g.spread),
      });
      break;
    }
    default:
      // An arm from a newer schema draws the fallback color of its writer.
      // Without one, the element that holds the paint is skipped.
      if (!p.hasFallback) throw new Skip();
      return { kind: "solid", color: rgbaFromU32(p.fallback) };
  }
  if (paint.kind === "solid") finiteNumber(paint.color.a);
  else {
    if (paint.kind === "linear") {
      finite([paint.x0, paint.y0, paint.x1, paint.y1]);
    } else finite([paint.cx, paint.cy, paint.radius]);
    for (const s of paint.stops) finite([s.offset, s.color.a]);
  }
  return paint;
}

// The solid color of a gradient with no stops or no extent, or the gradient.
function collapse(g: Paint & { kind: "linear" | "radial" }): Paint {
  if (g.stops.length === 0) return TRANSPARENT;
  const extent = g.kind === "linear"
    ? Math.hypot(g.x1 - g.x0, g.y1 - g.y0)
    : g.radius;
  if (extent <= NO_EXTENT) return lastStop(g.stops);
  return g;
}

const TRANSPARENT: Paint = { kind: "solid", color: { r: 0, g: 0, b: 0, a: 0 } };

function lastStop(stops: Stop[]): Paint {
  return { kind: "solid", color: stops[stops.length - 1].color };
}

// Raises a stop that is below the one before it and clamps every offset to
// [0, 1], as SVG and Skia do.
function readStops(list: $.List<W.Stop>): Stop[] {
  const stops: Stop[] = [];
  let prev = 0;
  for (let i = 0; i < list.length; i++) {
    const s = list.get(i);
    const offset = Math.min(Math.max(s.offset, prev), 1);
    stops.push({ offset, color: readRgba(s.color) });
    prev = offset;
  }
  return stops;
}

function readRgba(c: W.Rgba): Rgba {
  return { r: c.r, g: c.g, b: c.b, a: c.a };
}

// A 0xRRGGBBAA color, as the fallback of a paint carries it.
function rgbaFromU32(c: number): Rgba {
  return {
    r: (c >>> 24) & 0xff,
    g: (c >>> 16) & 0xff,
    b: (c >>> 8) & 0xff,
    a: (c & 0xff) / 255,
  };
}

// Rebuilds the segments, with the rules of read_segments in Rust. A path
// whose first verb is not a move begins at (0, 0), a move replaces a move
// before it, and a move that ends the path is dropped. An unknown verb is a
// value from a newer schema, and coords that do not match the verbs are
// damage.
function readSegments(verbData: $.Data, coordList: $.List<number>): Segments {
  const wire = verbData.toUint8Array();
  const coords = Float32Array.from(coordList.toArray());
  const verbs: number[] = [VERB_MOVE];
  const out: number[] = [0, 0];
  let i = 0;
  for (const verb of wire) {
    const n = coordCount(verb);
    if (i + n > coords.length) throw mismatch(wire, coords);
    if (verb === VERB_MOVE && verbs[verbs.length - 1] === VERB_MOVE) {
      out[out.length - 2] = coords[i];
      out[out.length - 1] = coords[i + 1];
    } else {
      verbs.push(verb);
      for (let k = 0; k < n; k++) out.push(coords[i + k]);
    }
    i += n;
  }
  if (i !== coords.length) throw mismatch(wire, coords);
  if (verbs[verbs.length - 1] === VERB_MOVE) {
    verbs.pop();
    out.length -= 2;
  }
  return {
    verbs: Uint8Array.from(verbs),
    coords: finite(Float32Array.from(out)),
  };
}

function coordCount(verb: number): number {
  switch (verb) {
    case VERB_MOVE:
    case VERB_LINE:
      return 2;
    case VERB_QUAD:
      return 4;
    case VERB_CUBIC:
      return 6;
    default:
      throw new Skip();
  }
}

function mismatch(verbs: Uint8Array, coords: Float32Array): Error {
  return new Error(
    `a path with ${verbs.length} verbs has ${coords.length} coords`,
  );
}

function lineCapOf(v: number): CanvasLineCap {
  return enumOf(v, ["butt", "round", "square"] as const);
}

function lineJoinOf(v: number): CanvasLineJoin {
  return enumOf(v, ["miter", "round", "bevel"] as const);
}

function fillRuleOf(v: number): CanvasFillRule {
  return enumOf(v, ["nonzero", "evenodd"] as const);
}

function fontStyleOf(v: number): TextElement["style"] {
  return enumOf(v, ["normal", "italic", "oblique"] as const);
}

function spreadOf(v: number): Spread {
  return enumOf(v, ["pad", "reflect", "repeat"] as const);
}

// The name of the enum value `v`, or a skip for a value from a newer schema.
function enumOf<T>(v: number, names: readonly T[]): T {
  if (v >= names.length) throw new Skip();
  return names[v];
}

function transformOf(
  m: { m0: number; m1: number; m2: number; m3: number; m4: number; m5: number },
): Transform {
  return finite([m.m0, m.m1, m.m2, m.m3, m.m4, m.m5]);
}

function finiteNumber(v: number): number {
  if (!Number.isFinite(v)) throw new Skip();
  return v;
}

function finite<T extends ArrayLike<number>>(values: T): T {
  for (let i = 0; i < values.length; i++) finiteNumber(values[i]);
  return values;
}

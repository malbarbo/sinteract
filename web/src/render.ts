// Draws a Scene on a canvas with the Canvas 2D API, which the browser
// rasterizes on the GPU. The scene fits the canvas, centered, and the band
// around it stays transparent, as the black band of the window.
//
// Two things of a scene have no direct form in Canvas 2D. The reflect and
// repeat spreads of a gradient become a pad gradient whose stops repeat over
// the visible area. A gradient of a text is in the space of the scene, so the
// text draws into a mask that the gradient then fills.

import {
  type Element,
  isVisible,
  type Paint,
  type PathStyle,
  type Rgba,
  type Scene,
  type Segments,
  type Spread,
  type Stop,
  type TextElement,
  type Transform,
  VERB_CUBIC,
  VERB_LINE,
  VERB_MOVE,
  VERB_QUAD,
} from "./scene.ts";

type Context = CanvasRenderingContext2D;

// A part of a text, which fills or strokes itself with the style of `ctx`.
type Shape = (ctx: Context, fill: boolean) => void;

// Where the scene goes on the canvas, in the pixels of the canvas.
export interface Placement {
  scale: number;
  x: number;
  y: number;
}

export class Renderer {
  #canvas: HTMLCanvasElement;
  #ctx: Context;
  // The canvases of the layers and of the masks, reused from frame to frame.
  #pool: HTMLCanvasElement[] = [];
  #images: (id: number) => CanvasImageSource | undefined = () => undefined;
  #place: Placement = { scale: 1, x: 0, y: 0 };
  #width = 0;
  #height = 0;

  constructor(canvas: HTMLCanvasElement) {
    this.#canvas = canvas;
    this.#ctx = context(canvas);
  }

  // Where the last scene went on the canvas.
  get place(): Placement {
    return this.#place;
  }

  // Draws `scene` fit to the canvas, with the image of each bitmap from
  // `images`. A bitmap whose image `images` does not give draws nothing.
  draw(
    scene: Scene,
    images: (id: number) => CanvasImageSource | undefined,
  ): void {
    const place = fit(
      scene.width,
      scene.height,
      this.#canvas.width,
      this.#canvas.height,
    );
    this.#images = images;
    this.#place = place;
    this.#width = scene.width;
    this.#height = scene.height;
    const ctx = this.#ctx;
    ctx.setTransform(1, 0, 0, 1, 0, 0);
    ctx.clearRect(0, 0, this.#canvas.width, this.#canvas.height);
    ctx.save();
    // The pixmap of the other displays has the size of the scene, so what
    // falls outside the scene does not draw.
    ctx.beginPath();
    ctx.rect(
      place.x,
      place.y,
      scene.width * place.scale,
      scene.height * place.scale,
    );
    ctx.clip();
    this.#elements(ctx, scene.elements);
    ctx.restore();
  }

  // Clears the canvas and forgets the placement of the last scene.
  clear(): void {
    this.#place = { scale: 1, x: 0, y: 0 };
    this.#ctx.setTransform(1, 0, 0, 1, 0, 0);
    this.#ctx.clearRect(0, 0, this.#canvas.width, this.#canvas.height);
  }

  #elements(ctx: Context, elements: Element[]): void {
    for (const e of elements) {
      switch (e.kind) {
        case "path":
          this.#path(ctx, e.style, toPath2D(e.segments, e.style.closed));
          break;
        case "clipped":
          ctx.save();
          this.#base(ctx);
          ctx.clip(toPath2D(e.clip, false), e.fillRule);
          this.#elements(ctx, e.elements);
          ctx.restore();
          break;
        case "text":
          this.#text(ctx, e);
          break;
        case "bitmap": {
          const image = this.#images(e.id);
          if (!image) break;
          this.#transform(ctx, e.transform);
          ctx.imageSmoothingEnabled = e.sampling === "smooth";
          ctx.imageSmoothingQuality = "high";
          ctx.drawImage(image, -0.5, -0.5, 1, 1);
          break;
        }
        case "layer": {
          const layer = this.#scratch();
          this.#elements(context(layer), e.elements);
          ctx.save();
          ctx.setTransform(1, 0, 0, 1, 0, 0);
          ctx.globalAlpha = e.opacity;
          ctx.drawImage(layer, 0, 0);
          ctx.restore();
          this.#pool.push(layer);
          break;
        }
      }
    }
  }

  #path(ctx: Context, style: PathStyle, path: Path2D): void {
    this.#base(ctx);
    if (isVisible(style.fill)) {
      ctx.fillStyle = this.#paint(ctx, style.fill);
      ctx.fill(path, style.fillRule);
    }
    if (isVisible(style.stroke) && style.strokeWidth > 0) {
      ctx.strokeStyle = this.#paint(ctx, style.stroke);
      ctx.lineWidth = style.strokeWidth;
      ctx.lineCap = style.lineCap;
      ctx.lineJoin = style.lineJoin;
      ctx.miterLimit = style.miterLimit;
      ctx.setLineDash(style.dash?.array ?? []);
      ctx.lineDashOffset = style.dash?.offset ?? 0;
      ctx.stroke(path);
    }
  }

  // Paints the glyphs, fill and then stroke, and then the underline the
  // same way, as the Rust renderers do.
  #text(ctx: Context, t: TextElement): void {
    const drawsFill = isVisible(t.fill);
    const drawsStroke = isVisible(t.stroke) && t.strokeWidth > 0;
    if (!(t.size > 0) || !(drawsFill || drawsStroke)) return;
    const face = faceOf(t.family, t.weight);
    const text = drawnText(t.text);
    const font = cssFont(t, face, t.size);
    setFont(ctx, font);
    const width = ctx.measureText(text).width;
    const em = t.size / UNITS_PER_EM;
    const left = -width / 2;
    const baseline = (face.ascender + face.descender) / 2 * em;
    // The position of the underline is its top, with y up.
    const top = baseline - face.underlinePosition * em;
    const thickness = face.underlineThickness * em;
    const glyphs: Shape = (c, fill) => {
      setFont(c, font);
      c.textAlign = "left";
      c.textBaseline = "alphabetic";
      if (fill) c.fillText(text, left, baseline);
      else c.strokeText(text, left, baseline);
    };
    const underline: Shape = (c, fill) => {
      if (fill) c.fillRect(left, top, width, thickness);
      else c.strokeRect(left, top, width, thickness);
    };
    for (const shape of t.underline ? [glyphs, underline] : [glyphs]) {
      if (drawsFill) this.#textPart(ctx, t, shape, t.fill, true);
      if (drawsStroke) this.#textPart(ctx, t, shape, t.stroke, false);
    }
  }

  // Fills or strokes `shape` of `t` with `paint`. A gradient goes through a
  // mask of the shape.
  #textPart(
    ctx: Context,
    t: TextElement,
    shape: Shape,
    paint: Paint,
    fill: boolean,
  ): void {
    const mask = paint.kind === "solid" ? null : this.#scratch();
    const c = mask ? context(mask) : ctx;
    const color = paint.kind === "solid" ? cssColor(paint.color) : "#000";
    this.#transform(c, t.transform);
    if (fill) c.fillStyle = color;
    else {
      c.strokeStyle = color;
      c.lineWidth = t.strokeWidth;
      c.lineJoin = "miter";
      c.miterLimit = TEXT_MITER_LIMIT;
      c.setLineDash([]);
    }
    shape(c, fill);
    if (!mask) return;
    c.globalCompositeOperation = "source-in";
    this.#base(c);
    c.fillStyle = this.#paint(c, paint);
    c.fillRect(0, 0, this.#width, this.#height);
    ctx.save();
    ctx.setTransform(1, 0, 0, 1, 0, 0);
    ctx.drawImage(mask, 0, 0);
    ctx.restore();
    this.#pool.push(mask);
  }

  // A canvas style for `paint`, in the space of the scene.
  #paint(ctx: Context, paint: Paint): string | CanvasGradient {
    if (paint.kind === "solid") return cssColor(paint.color);
    const [t0, t1] = this.#visibleRange(paint);
    const stops = spreadStops(paint.stops, paint.spread, t0, t1);
    let g: CanvasGradient;
    if (paint.kind === "linear") {
      const { x0, y0, x1, y1 } = paint;
      const at = (t: number) => [x0 + (x1 - x0) * t, y0 + (y1 - y0) * t];
      const [ax, ay] = at(t0);
      const [bx, by] = at(t1);
      g = ctx.createLinearGradient(ax, ay, bx, by);
    } else {
      const { cx, cy, radius } = paint;
      g = ctx.createRadialGradient(cx, cy, 0, cx, cy, radius * t1);
    }
    for (const s of stops) g.addColorStop(s.offset, cssColor(s.color));
    return g;
  }

  // The whole periods of the ramp of `paint` that the scene covers, as the
  // first and the last value of t, where the ramp spans t in [0, 1]. A pad
  // gradient needs no more than its own ramp.
  #visibleRange(
    paint: Paint & { kind: "linear" | "radial" },
  ): [number, number] {
    if (paint.spread === "pad") return [0, 1];
    const corners = [[0, 0], [this.#width, 0], [0, this.#height], [
      this.#width,
      this.#height,
    ]];
    let lo = Infinity, hi = -Infinity;
    for (const [x, y] of corners) {
      let t: number;
      if (paint.kind === "linear") {
        const dx = paint.x1 - paint.x0, dy = paint.y1 - paint.y0;
        t = ((x - paint.x0) * dx + (y - paint.y0) * dy) / (dx * dx + dy * dy);
      } else {
        t = Math.hypot(x - paint.cx, y - paint.cy) / paint.radius;
      }
      lo = Math.min(lo, t);
      hi = Math.max(hi, t);
    }
    if (paint.kind === "radial") lo = 0;
    lo = Math.floor(lo);
    hi = Math.max(Math.ceil(hi), lo + 1);
    // Past this many periods a stripe is thinner than a pixel of any scene
    // this client shows, so the ramp stops repeating and pads.
    if (hi - lo > MAX_PERIODS) {
      const start = Math.min(Math.max(lo, -MAX_PERIODS / 2), hi - MAX_PERIODS);
      return [start, start + MAX_PERIODS];
    }
    return [lo, hi];
  }

  // The transform of the scene, from the placement.
  #base(ctx: Context): void {
    const { scale, x, y } = this.#place;
    ctx.setTransform(scale, 0, 0, scale, x, y);
  }

  // The transform `m` of an element, after the transform of the scene.
  #transform(ctx: Context, m: Transform): void {
    const { scale: s, x, y } = this.#place;
    ctx.setTransform(
      s * m[0],
      s * m[1],
      s * m[2],
      s * m[3],
      s * m[4] + x,
      s * m[5] + y,
    );
  }

  // A transparent canvas of the size of the canvas, from the pool.
  #scratch(): HTMLCanvasElement {
    const c = this.#pool.pop() ?? document.createElement("canvas");
    const ctx = context(c);
    if (c.width !== this.#canvas.width || c.height !== this.#canvas.height) {
      c.width = this.#canvas.width;
      c.height = this.#canvas.height;
    }
    ctx.globalCompositeOperation = "source-over";
    ctx.globalAlpha = 1;
    ctx.setTransform(1, 0, 0, 1, 0, 0);
    ctx.clearRect(0, 0, c.width, c.height);
    return c;
  }
}

// The placement that fits a scene of `width` by `height` in a canvas of
// `canvasWidth` by `canvasHeight`, centered, at any scale.
function fit(
  width: number,
  height: number,
  canvasWidth: number,
  canvasHeight: number,
): Placement {
  if (width <= 0 || height <= 0) return { scale: 1, x: 0, y: 0 };
  const scale = Math.min(canvasWidth / width, canvasHeight / height);
  return {
    scale,
    x: (canvasWidth - width * scale) / 2,
    y: (canvasHeight - height * scale) / 2,
  };
}

const MAX_PERIODS = 256;

// The miter limit of the stroke of a text, as in the Rust renderers.
const TEXT_MITER_LIMIT = 10;

function context(canvas: HTMLCanvasElement): Context {
  const ctx = canvas.getContext("2d");
  if (!ctx) throw new Error("the browser gives no 2d context");
  return ctx;
}

// The stops of a pad gradient over t in [t0, t1] that draw the ramp of
// `stops` with `spread` there. Period k of the ramp spans t in [k, k + 1],
// and reflect runs the odd periods backward.
function spreadStops(
  stops: Stop[],
  spread: Spread,
  t0: number,
  t1: number,
): Stop[] {
  const backward = stops
    .map((s) => ({ offset: 1 - s.offset, color: s.color }))
    .reverse();
  const out: Stop[] = [];
  const span = t1 - t0;
  for (let k = t0; k < t1; k++) {
    const odd = Math.abs(k % 2) === 1;
    for (const s of spread === "reflect" && odd ? backward : stops) {
      out.push({ offset: (k - t0 + s.offset) / span, color: s.color });
    }
  }
  return out;
}

function toPath2D(segments: Segments, closed: boolean): Path2D {
  const path = new Path2D();
  const c = segments.coords;
  let i = 0;
  let open = false;
  for (const verb of segments.verbs) {
    switch (verb) {
      case VERB_MOVE:
        if (closed && open) path.closePath();
        path.moveTo(c[i], c[i + 1]);
        i += 2;
        break;
      case VERB_LINE:
        path.lineTo(c[i], c[i + 1]);
        i += 2;
        break;
      case VERB_QUAD:
        path.quadraticCurveTo(c[i], c[i + 1], c[i + 2], c[i + 3]);
        i += 4;
        break;
      case VERB_CUBIC:
        path.bezierCurveTo(
          c[i],
          c[i + 1],
          c[i + 2],
          c[i + 3],
          c[i + 4],
          c[i + 5],
        );
        i += 6;
        break;
    }
    open = verb !== VERB_MOVE || open;
  }
  if (closed && open) path.closePath();
  return path;
}

function cssColor(c: Rgba): string {
  return `rgba(${c.r},${c.g},${c.b},${c.a / 255})`;
}

// The metrics of a face of the Sinteract fonts, which the Rust side embeds
// and measures with, in font units. The page loads them as web fonts from
// fonts/, beside the page. They have the outlines of the Liberation fonts,
// so the CSS font of each family names next the Liberation font of the
// system and then the fonts with the same metrics, and the text measures as
// it does in the engine when the web fonts do not load.
interface Face {
  css: string;
  ascender: number;
  descender: number;
  underlinePosition: number;
  underlineThickness: number;
}

const UNITS_PER_EM = 2048;

const SANS =
  '"Sinteract Sans", "Liberation Sans", Arimo, Arial, Helvetica, sans-serif';
const SERIF =
  '"Sinteract Serif", "Liberation Serif", Tinos, "Times New Roman", Times, serif';
const MONO =
  '"Sinteract Mono", "Liberation Mono", Cousine, "Courier New", Courier, monospace';

// From the hhea and post tables of the fonts in ../fonts. Regular and bold
// differ only in the underline, as the position and the thickness of each.
// The italic of each family has the metrics of the upright one.
const FAMILIES = {
  sans: {
    css: SANS,
    ascender: 1854,
    descender: -434,
    underline: [[-67, 150], [-2, 215]],
  },
  serif: {
    css: SERIF,
    ascender: 1825,
    descender: -443,
    underline: [[-123, 100], [-28, 195]],
  },
  mono: {
    css: MONO,
    ascender: 1705,
    descender: -615,
    underline: [[-393, 84], [-272, 205]],
  },
};

// A weight from 600 up is bold, as in text.rs.
const BOLD = 600;

// The families by the aliases of ResolvedFont::resolve in Rust.
const ALIASES: Record<string, typeof FAMILIES.sans> = {
  "": FAMILIES.sans,
  "sans-serif": FAMILIES.sans,
  "sans": FAMILIES.sans,
  "sinteract sans": FAMILIES.sans,
  "serif": FAMILIES.serif,
  "sinteract serif": FAMILIES.serif,
  "monospace": FAMILIES.mono,
  "mono": FAMILIES.mono,
  "sinteract mono": FAMILIES.mono,
};

// The face of a family. A family of another name draws with that font when
// the browser has it, and measures as Sinteract Sans, the face that the
// Rust view falls back to.
function faceOf(family: string, weight: number): Face {
  const name = family.trim();
  const known = Object.hasOwn(ALIASES, name.toLowerCase())
    ? ALIASES[name.toLowerCase()]
    : null;
  const f = known ?? FAMILIES.sans;
  const [underlinePosition, underlineThickness] =
    f.underline[weight >= BOLD ? 1 : 0];
  return {
    css: known ? f.css : `${JSON.stringify(name)}, ${SANS}`,
    ascender: f.ascender,
    descender: f.descender,
    underlinePosition,
    underlineThickness,
  };
}

// The CSS font of `t` at 16 pixels, which picks the same faces as the font
// that draws `t`, for the FontFaceSet to load.
export function fontToLoad(t: TextElement): string {
  return cssFont(t, faceOf(t.family, t.weight), 16);
}

function cssFont(t: TextElement, face: Face, size: number): string {
  const style = t.style === "normal" ? "normal" : "italic";
  const weight = t.weight >= BOLD ? "bold" : "normal";
  return `${style} ${weight} ${size}px ${face.css}`;
}

// Sets `font` with the advances of the font as they are. The engine does
// not kern, and with the default text rendering Chrome on Linux rounds each
// advance to a whole pixel, which makes a small text wider.
function setFont(ctx: Context, font: string): void {
  ctx.font = font;
  ctx.fontKerning = "none";
  ctx.textRendering = "geometricPrecision";
}

// A tab advances by eight spaces, and any other control character draws
// nothing, so a newline does not break the line.
function drawnText(text: string): string {
  return text.replaceAll("\t", "        ").replace(/\p{Cc}/gu, "");
}

//! The backend that draws a scene on an HTML canvas with the Canvas 2D API
//! of the browser, through web-sys. It builds only on wasm32.
//!
//! The page keeps the size of the canvas, and the scene fits it, centered,
//! as in the window. The glyphs are the outlines of the embedded fonts, as
//! in the other backends, so a text draws with the widths from the engine.
//! The browser decodes the images, so the module needs no decoder of its
//! own.

use std::cell::RefCell;
use std::collections::HashMap;
use std::convert::Infallible;
use std::hash::Hash;
use std::rc::Rc;

use js_sys::{Array, Promise, Uint8Array};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::prelude::wasm_bindgen;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{
    Blob, CanvasGradient, CanvasRenderingContext2d, CanvasWindingRule, ColorSpaceConversion,
    HtmlCanvasElement, ImageBitmap, ImageBitmapOptions, Path2d,
};

use crate::outline::PathSink;
use crate::renderer::{
    MISSING_FILL, MISSING_STROKE, Renderer, RestoreOnDrop, TEXT_MITER_LIMIT, frame_side,
    missing_box, missing_cross, sealed::Canvas,
};
use crate::scene::{
    Bitmap, ClipPath, Element, FillRule, Gradient, GradientGeometry, Image, LineCap, LineJoin,
    Paint, Path, Rgba, Sampling, Scene, SpreadMode, Text,
};
use crate::text::{Glyph, TextLayout};

/// A renderer that draws on an [`HtmlCanvasElement`].
pub struct CanvasRenderer {
    canvas: HtmlCanvasElement,
    main: CanvasRenderingContext2d,
    /// The open layers. The last one receives the drawing.
    layers: Vec<Layer>,
    /// The canvases of the closed layers, for the next layers.
    pool: Vec<HtmlCanvasElement>,
    place: Placement,
    /// The size of the scene, for the periods of a gradient.
    width: f32,
    height: f32,
    glyphs: FrameCache<Glyph, Option<Path2d>>,
    /// The decode of each image, which the browser fills in later. An image
    /// that does not decode keeps [`Decode::Failed`], so it draws the marker
    /// of a missing image with no second decode.
    images: FrameCache<Image, Rc<RefCell<Decode>>>,
}

/// Where the decode of an image is.
enum Decode {
    /// The browser decodes it, and the promise resolves after that, in
    /// success or in failure.
    Pending(Promise),
    Ready(ImageBitmap),
    Failed,
}

/// Where the scene goes on the canvas, in the pixels of the canvas. A point
/// (x, y) of the scene goes to (x × scale + self.x, y × scale + self.y).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    pub scale: f32,
    pub x: f32,
    pub y: f32,
}

impl CanvasRenderer {
    /// A renderer that draws on `canvas`. It panics if the canvas already
    /// has a context other than a 2D one.
    pub fn new(canvas: HtmlCanvasElement) -> Self {
        let main = context(&canvas);
        CanvasRenderer {
            canvas,
            main,
            layers: Vec::new(),
            pool: Vec::new(),
            place: Placement {
                scale: 1.0,
                x: 0.0,
                y: 0.0,
            },
            width: 1.0,
            height: 1.0,
            glyphs: FrameCache::default(),
            images: FrameCache::default(),
        }
    }

    /// Start to decode each image of `scene` that the renderer does not hold.
    /// Returns a promise that resolves when they all decoded or failed, or
    /// `None` if none of them is pending. The browser decodes an image
    /// asynchronously, and a render draws nothing for a pending image, so
    /// a caller that does not want a frame with a hole waits for the
    /// promise. An image stays while the renders or the loads use it, so a
    /// caller that waits calls `load` before each render of an older scene.
    pub fn load(&mut self, scene: &Scene) -> Option<Promise> {
        let pending = Array::new();
        let mut visit = |image: &Image| {
            if let Decode::Pending(p) = &*self.decode(image).borrow() {
                pending.push(p);
            }
        };
        for_each_image(scene.elements(), &mut visit);
        (pending.length() > 0).then(|| Promise::all(&pending))
    }

    /// Where the last scene went on the canvas.
    pub fn place(&self) -> Placement {
        self.place
    }

    /// Clear the canvas.
    pub fn clear(&self) {
        _ = self.main.reset_transform();
        self.main.clear_rect(
            0.0,
            0.0,
            f64::from(self.canvas.width()),
            f64::from(self.canvas.height()),
        );
    }

    fn ctx(&self) -> &CanvasRenderingContext2d {
        self.layers.last().map_or(&self.main, |l| &l.ctx)
    }

    /// Set the transform of the context to `m` in the space of the scene.
    fn set(&self, m: [f32; 6]) {
        let Placement { scale, x, y } = self.place;
        let [a, b, c, d, e, f] = m;
        _ = self.ctx().set_transform(
            f64::from(scale * a),
            f64::from(scale * b),
            f64::from(scale * c),
            f64::from(scale * d),
            f64::from(scale * e + x),
            f64::from(scale * f + y),
        );
    }

    fn fill_style(&self, paint: &Paint) {
        match paint {
            Paint::Solid(c) => self.ctx().set_fill_style_str(&css(*c)),
            Paint::Gradient(g) => self.ctx().set_fill_style_canvas_gradient(&self.gradient(g)),
        }
    }

    fn stroke_style(&self, paint: &Paint) {
        match paint {
            Paint::Solid(c) => self.ctx().set_stroke_style_str(&css(*c)),
            Paint::Gradient(g) => self
                .ctx()
                .set_stroke_style_canvas_gradient(&self.gradient(g)),
        }
    }

    /// The gradient in the space of the scene. Canvas 2D has only the pad
    /// spread, so reflect and repeat become a pad gradient whose stops
    /// repeat over the periods that the scene shows.
    fn gradient(&self, g: &Gradient) -> CanvasGradient {
        let corners = [
            (0.0, 0.0),
            (self.width, 0.0),
            (0.0, self.height),
            (self.width, self.height),
        ];
        let (t0, t1, grad) = match g.geometry() {
            GradientGeometry::Linear { x0, y0, x1, y1 } => {
                let (dx, dy) = (x1 - x0, y1 - y0);
                let len2 = dx * dx + dy * dy;
                let ts = corners.map(|(x, y)| ((x - x0) * dx + (y - y0) * dy) / len2);
                let (t0, t1) = periods(g.spread(), ts);
                let grad = self.ctx().create_linear_gradient(
                    f64::from(x0 + t0 * dx),
                    f64::from(y0 + t0 * dy),
                    f64::from(x0 + t1 * dx),
                    f64::from(y0 + t1 * dy),
                );
                (t0, t1, grad)
            }
            GradientGeometry::Radial { cx, cy, radius } => {
                let ts = corners.map(|(x, y)| (x - cx).hypot(y - cy) / radius);
                // A radial gradient starts at the center, so its periods
                // start at 0.
                let (_, t1) = periods(g.spread(), ts);
                let grad = self
                    .ctx()
                    .create_radial_gradient(
                        f64::from(cx),
                        f64::from(cy),
                        0.0,
                        f64::from(cx),
                        f64::from(cy),
                        f64::from(radius * t1),
                    )
                    .expect("a radius of the scene is finite and positive");
                (0.0, t1, grad)
            }
        };
        let span = t1 - t0;
        let mut k = t0;
        while k < t1 {
            let reverse = g.spread() == SpreadMode::Reflect && (k as i32).rem_euclid(2) == 1;
            let add = |offset: f32, color: Rgba| {
                let at = ((k + offset - t0) / span).clamp(0.0, 1.0);
                _ = grad.add_color_stop(at, &css(color));
            };
            if reverse {
                let stops: Vec<_> = g.stops().iter().collect();
                for s in stops.iter().rev() {
                    add(1.0 - s.offset, s.color);
                }
            } else {
                for s in g.stops() {
                    add(s.offset, s.color);
                }
            }
            k += 1.0;
        }
        grad
    }

    /// The path of `glyph`, or `None` for a glyph with no outline, such as
    /// a space.
    fn glyph(&mut self, glyph: Glyph) -> Option<Path2d> {
        self.glyphs
            .get_or_insert_with(glyph, || {
                let mut sink = Calls::new();
                glyph.outline(0.0, 0.0, &mut sink);
                (!sink.empty).then_some(sink.path)
            })
            .clone()
    }

    fn decode(&mut self, image: &Image) -> Rc<RefCell<Decode>> {
        self.images
            .get_or_insert_with(image.clone(), || start_decode(image))
            .clone()
    }

    fn text_stroke(&self, width: f32) {
        let ctx = self.ctx();
        ctx.set_line_width(f64::from(width));
        ctx.set_line_join("miter");
        ctx.set_line_cap("butt");
        ctx.set_miter_limit(f64::from(TEXT_MITER_LIMIT));
        _ = ctx.set_line_dash(&js_sys::Array::new());
    }

    /// Draw a gray box with a red cross, one pixel wide, over the unit
    /// square of `t`.
    fn draw_missing(&self, t: [f32; 6]) {
        let mut outline = Calls::new();
        missing_box(t, &mut outline);
        let mut cross = Calls::new();
        missing_cross(t, &mut cross);
        self.set(IDENTITY);
        let ctx = self.ctx();
        ctx.set_fill_style_str(&css(MISSING_FILL));
        ctx.fill_with_path_2d(&outline.path);
        ctx.set_stroke_style_str(&css(MISSING_STROKE));
        ctx.set_line_width(f64::from(1.0 / self.place.scale));
        ctx.set_line_join("miter");
        ctx.set_line_cap("butt");
        _ = ctx.set_line_dash(&js_sys::Array::new());
        ctx.stroke_with_path(&outline.path);
        ctx.stroke_with_path(&cross.path);
    }
}

impl Canvas<Infallible> for CanvasRenderer {
    /// Fits the scene to the canvas, clears the canvas and clips it to the
    /// scene, as the pixmap of the other displays has the size of the scene.
    fn ensure_size(&mut self, width: f32, height: f32) -> Result<(), Infallible> {
        self.width = frame_side(width);
        self.height = frame_side(height);
        let (cw, ch) = (self.canvas.width() as f32, self.canvas.height() as f32);
        let scale = (cw / self.width).min(ch / self.height);
        self.place = Placement {
            scale,
            x: (cw - self.width * scale) / 2.0,
            y: (ch - self.height * scale) / 2.0,
        };
        self.glyphs.next_frame();
        self.images.next_frame();
        self.clear();
        let ctx = self.ctx();
        ctx.save();
        self.set(IDENTITY);
        ctx.begin_path();
        ctx.rect(0.0, 0.0, f64::from(self.width), f64::from(self.height));
        ctx.clip();
        Ok(())
    }

    fn end_frame(&mut self) {
        self.ctx().restore();
    }

    fn draw_path(&mut self, path: &Path) {
        let style = &path.style;
        let fill = style.draws_fill();
        let stroke = style.draws_stroke();
        if !fill && !stroke {
            return;
        }
        let mut sink = Calls::new();
        path.outline(&mut sink);
        if sink.empty {
            return;
        }
        let p = sink.path;
        self.set(IDENTITY);
        let ctx = self.ctx();
        if fill {
            self.fill_style(&style.fill);
            ctx.fill_with_path_2d_and_winding(&p, winding(style.fill_rule));
        }
        if stroke {
            self.stroke_style(&style.stroke);
            ctx.set_line_width(f64::from(style.stroke_width));
            ctx.set_line_cap(match style.line_cap {
                LineCap::Butt => "butt",
                LineCap::Round => "round",
                LineCap::Square => "square",
            });
            ctx.set_line_join(match style.line_join {
                LineJoin::Miter => "miter",
                LineJoin::Round => "round",
                LineJoin::Bevel => "bevel",
            });
            ctx.set_miter_limit(f64::from(style.miter_limit));
            let dash = js_sys::Array::new();
            if let Some(d) = &style.dash {
                for v in d.array() {
                    dash.push(&JsValue::from_f64(f64::from(*v)));
                }
                ctx.set_line_dash_offset(f64::from(d.offset()));
            }
            _ = ctx.set_line_dash(&dash);
            ctx.stroke_with_path(&p);
        }
    }

    fn draw_text(&mut self, node: &Text) {
        let Some(layout) = TextLayout::new(&node.spec) else {
            return;
        };
        let fill = node.draws_fill();
        let stroke = node.draws_stroke();
        if !fill && !stroke {
            return;
        }
        let gradient =
            matches!(node.fill, Paint::Gradient(_)) || matches!(node.stroke, Paint::Gradient(_));
        if gradient {
            // The gradient stays in the space of the scene, so the glyphs
            // go into one path under the transform of the text.
            let mut moved = Moved {
                out: Calls::new(),
                t: node.transform,
            };
            layout.outline(&mut moved);
            if node.underline {
                layout.outline_underline(&mut moved);
            }
            let p = moved.out.path;
            self.set(IDENTITY);
            if fill {
                self.fill_style(&node.fill);
                self.ctx().fill_with_path_2d(&p);
            }
            if stroke {
                self.stroke_style(&node.stroke);
                self.text_stroke(node.stroke_width);
                self.ctx().stroke_with_path(&p);
            }
            return;
        }
        // Each glyph keeps its path from frame to frame, and draws under the
        // transform of the text moved to its origin.
        let placed: Vec<_> = layout
            .placed_glyphs()
            .filter_map(|(g, x)| Some((self.glyph(g)?, x)))
            .collect();
        let y = layout.baseline_y();
        let [a, b, c, d, e, f] = node.transform;
        let at = |x: f32| [a, b, c, d, a * x + c * y + e, b * x + d * y + f];
        let underline = node.underline.then(|| {
            let mut sink = Calls::new();
            layout.outline_underline(&mut sink);
            sink.path
        });
        if fill {
            self.fill_style(&node.fill);
            for (p, x) in &placed {
                self.set(at(*x));
                self.ctx().fill_with_path_2d(p);
            }
        }
        if stroke {
            self.stroke_style(&node.stroke);
            self.text_stroke(node.stroke_width);
            for (p, x) in &placed {
                self.set(at(*x));
                self.ctx().stroke_with_path(p);
            }
        }
        if let Some(u) = underline {
            self.set(node.transform);
            if fill {
                self.fill_style(&node.fill);
                self.ctx().fill_with_path_2d(&u);
            }
            if stroke {
                self.stroke_style(&node.stroke);
                self.ctx().stroke_with_path(&u);
            }
        }
    }

    /// Draw the image of `bitmap`, or nothing while it decodes. An image
    /// that does not decode draws a gray box with a red cross in its place,
    /// as in the pixmap.
    fn draw_bitmap(&mut self, bitmap: &Bitmap) {
        let decode = self.decode(&bitmap.image);
        let img = match &*decode.borrow() {
            Decode::Ready(img) => img.clone(),
            Decode::Pending(_) => return,
            Decode::Failed => {
                self.draw_missing(bitmap.transform);
                return;
            }
        };
        self.set(bitmap.transform);
        let ctx = self.ctx();
        ctx.set_image_smoothing_enabled(bitmap.sampling == Sampling::Smooth);
        _ = ctx.draw_image_with_image_bitmap_and_dw_and_dh(&img, -0.5, -0.5, 1.0, 1.0);
    }

    fn with_clip<T>(&mut self, clip: &ClipPath, inside: impl FnOnce(&mut Self) -> T) -> T {
        let mut sink = Calls::new();
        clip.segments().outline(&mut sink);
        let ctx = self.ctx();
        ctx.save();
        self.set(IDENTITY);
        ctx.clip_with_path_2d_and_winding(&sink.path, winding(clip.fill_rule));
        let guard = RestoreOnDrop {
            canvas: self,
            restore: |c: &mut Self| c.ctx().restore(),
        };
        inside(&mut *guard.canvas)
    }

    fn with_layer<T>(&mut self, opacity: f32, inside: impl FnOnce(&mut Self) -> T) -> T {
        let canvas = self.pool.pop().unwrap_or_else(new_canvas);
        let (w, h) = (self.canvas.width(), self.canvas.height());
        if canvas.width() != w || canvas.height() != h {
            canvas.set_width(w);
            canvas.set_height(h);
        }
        let ctx = context(&canvas);
        _ = ctx.reset_transform();
        ctx.clear_rect(0.0, 0.0, f64::from(w), f64::from(h));
        self.layers.push(Layer {
            canvas,
            ctx,
            opacity,
        });
        let guard = RestoreOnDrop {
            canvas: self,
            restore: |c: &mut Self| {
                let Some(layer) = c.layers.pop() else {
                    return;
                };
                // The clips in effect already masked what went into the
                // layer, and the layer has the size of the canvas.
                let parent = c.ctx();
                parent.save();
                _ = parent.reset_transform();
                parent.set_global_alpha(f64::from(layer.opacity));
                _ = parent.draw_image_with_html_canvas_element(&layer.canvas, 0.0, 0.0);
                parent.restore();
                c.pool.push(layer.canvas);
            },
        };
        inside(&mut *guard.canvas)
    }
}

impl Renderer for CanvasRenderer {
    type Error = Infallible;
    type Output<'a> = ();

    /// The frame is on the canvas, so there is nothing to borrow.
    fn output(&self) {}
}

/// A layer that is open, with the canvas that receives its elements.
struct Layer {
    canvas: HtmlCanvasElement,
    ctx: CanvasRenderingContext2d,
    opacity: f32,
}

const IDENTITY: [f32; 6] = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

/// The values that the last frame and this one used. A value that no frame
/// used for a whole frame goes, so the cache stays the size of what one
/// frame draws.
struct FrameCache<K, V> {
    this: HashMap<K, V>,
    last: HashMap<K, V>,
}

impl<K, V> Default for FrameCache<K, V> {
    fn default() -> Self {
        FrameCache {
            this: HashMap::new(),
            last: HashMap::new(),
        }
    }
}

impl<K: Eq + Hash, V> FrameCache<K, V> {
    fn next_frame(&mut self) {
        self.last = std::mem::take(&mut self.this);
    }

    fn get_or_insert_with(&mut self, key: K, make: impl FnOnce() -> V) -> &V {
        let value = self.last.remove(&key);
        self.this
            .entry(key)
            .or_insert_with(|| value.unwrap_or_else(make))
    }
}

/// The first and the last period of the gradient that the corners `ts`
/// reach, rounded out to whole periods. At most 256 periods draw, the last
/// ones.
fn periods(spread: SpreadMode, ts: [f32; 4]) -> (f32, f32) {
    if spread == SpreadMode::Pad {
        return (0.0, 1.0);
    }
    let lo = ts
        .iter()
        .copied()
        .fold(f32::INFINITY, f32::min)
        .floor()
        .min(0.0);
    let hi = ts
        .iter()
        .copied()
        .fold(f32::NEG_INFINITY, f32::max)
        .ceil()
        .max(1.0);
    (lo.max(hi - 256.0), hi)
}

fn winding(rule: FillRule) -> CanvasWindingRule {
    match rule {
        FillRule::NonZero => CanvasWindingRule::Nonzero,
        FillRule::EvenOdd => CanvasWindingRule::Evenodd,
    }
}

/// The color as `#rrggbbaa`, which carries the alpha byte as it is.
fn css(c: Rgba) -> String {
    format!("#{:02x}{:02x}{:02x}{:02x}", c.r, c.g, c.b, c.a)
}

/// Call `visit` with the image of each bitmap of `elements`, and of what
/// their clips and layers hold.
fn for_each_image(elements: &[Element], visit: &mut impl FnMut(&Image)) {
    for e in elements {
        match e {
            Element::Bitmap(b) => visit(&b.image),
            Element::Clipped { elements, .. } | Element::Layer { elements, .. } => {
                for_each_image(elements, visit);
            }
            Element::Path(_) | Element::Text(_) => {}
        }
    }
}

/// Ask the browser to decode `image`. The decode fills in the slot that it
/// returns. The image ignores its color profile, as in the other backends.
fn start_decode(image: &Image) -> Rc<RefCell<Decode>> {
    let slot = Rc::new(RefCell::new(Decode::Failed));
    let parts = Array::of1(&Uint8Array::from(image.blob()));
    let options = ImageBitmapOptions::new();
    options.set_color_space_conversion(ColorSpaceConversion::None);
    let Some(decoding) = Blob::new_with_u8_array_sequence(&parts)
        .ok()
        .and_then(|blob| {
            web_sys::window()?
                .create_image_bitmap_with_blob_and_image_bitmap_options(&blob, &options)
                .ok()
        })
    else {
        return slot;
    };
    // One function takes both the bitmap and the error, and the promise
    // calls it once, which frees it.
    let settle = {
        let slot = Rc::clone(&slot);
        Closure::once_into_js(move |value: JsValue| {
            *slot.borrow_mut() = value.dyn_into().map_or(Decode::Failed, Decode::Ready);
        })
    };
    *slot.borrow_mut() = Decode::Pending(
        decoding
            .unchecked_ref::<Thenable>()
            .then_settle(&settle, &settle),
    );
    slot
}

#[wasm_bindgen]
extern "C" {
    /// A promise seen through `then` with two plain functions, which
    /// [`Promise::then2`] does not take.
    type Thenable;

    #[wasm_bindgen(method, js_name = then)]
    fn then_settle(this: &Thenable, ok: &JsValue, err: &JsValue) -> Promise;
}

fn new_canvas() -> HtmlCanvasElement {
    web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.create_element("canvas").ok())
        .and_then(|e| e.dyn_into().ok())
        .expect("the renderer runs in a page")
}

fn context(canvas: &HtmlCanvasElement) -> CanvasRenderingContext2d {
    canvas
        .get_context("2d")
        .ok()
        .flatten()
        .and_then(|c| c.dyn_into().ok())
        .expect("the canvas has a 2D context")
}

/// Builds a [`Path2d`] with one call for each segment.
struct Calls {
    path: Path2d,
    /// `true` until the first segment, so an outline with none draws
    /// nothing.
    empty: bool,
}

impl Calls {
    fn new() -> Self {
        Calls {
            path: Path2d::new().expect("a page makes a Path2D"),
            empty: true,
        }
    }
}

impl PathSink for Calls {
    fn move_to(&mut self, x: f32, y: f32) {
        self.empty = false;
        self.path.move_to(f64::from(x), f64::from(y));
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.path.line_to(f64::from(x), f64::from(y));
    }
    fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        self.path
            .quadratic_curve_to(f64::from(cx), f64::from(cy), f64::from(x), f64::from(y));
    }
    fn cubic_to(&mut self, cx1: f32, cy1: f32, cx2: f32, cy2: f32, x: f32, y: f32) {
        self.path.bezier_curve_to(
            f64::from(cx1),
            f64::from(cy1),
            f64::from(cx2),
            f64::from(cy2),
            f64::from(x),
            f64::from(y),
        );
    }
    fn close(&mut self) {
        self.path.close_path();
    }
}

/// Passes an outline on to `out` under the affine `t`.
struct Moved {
    out: Calls,
    t: [f32; 6],
}

impl Moved {
    fn p(&self, x: f32, y: f32) -> (f32, f32) {
        let [a, b, c, d, e, f] = self.t;
        (a * x + c * y + e, b * x + d * y + f)
    }
}

impl PathSink for Moved {
    fn move_to(&mut self, x: f32, y: f32) {
        let (x, y) = self.p(x, y);
        self.out.move_to(x, y);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        let (x, y) = self.p(x, y);
        self.out.line_to(x, y);
    }
    fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        let (cx, cy) = self.p(cx, cy);
        let (x, y) = self.p(x, y);
        self.out.quad_to(cx, cy, x, y);
    }
    fn cubic_to(&mut self, cx1: f32, cy1: f32, cx2: f32, cy2: f32, x: f32, y: f32) {
        let (cx1, cy1) = self.p(cx1, cy1);
        let (cx2, cy2) = self.p(cx2, cy2);
        let (x, y) = self.p(x, y);
        self.out.cubic_to(cx1, cy1, cx2, cy2, x, y);
    }
    fn close(&mut self) {
        self.out.close();
    }
}

//! [`Renderer`] — the trait every renderer implements.
//!
//! [`crate::scene::Scene::render`] walks a draw-list once and dispatches
//! each element through this trait. Each renderer (raster, PDF, future SVG)
//! is responsible only for its own backend; the scene owns the format and
//! the SVG-arc → cubic conversion, so the trait surface stays small.
//!
//! Coordinates are in CSS pixels with y-down / top-left origin, matching the
//! draw-list wire format. Backends that need a different convention apply a
//! transform once at the start (see `pdf` and `terminal`).
//!
//! ## Three entry points (safe API)
//!
//! * [`Renderer::render_scene`] — atomic: replay a fully-built
//!   [`crate::scene::Scene`]. Object-safe.
//! * [`Renderer::render_scene_stream`] — streaming: decode one Cap'n Proto
//!   `Frame` message from a [`std::io::Read`] source and dispatch it
//!   through the renderer's primitives without materializing an
//!   intermediate `Scene`. Per-path buffering bounded by the path itself;
//!   the scene-level `Vec<Element>` is never built.
//! * [`Renderer::path`] / [`Renderer::clip`] — RAII builder API:
//!   `renderer.path(style)` returns a [`PathScope`] that brackets
//!   `path_begin`/`path_end`; `renderer.clip(c)` returns a [`ClipScope`]
//!   that brackets `clip_push`/`clip_pop`. Same shape as
//!   [`crate::scene::PathBuilder`]/[`crate::scene::ClipBuilder`] on the
//!   scene side.
//!
//! ## Backend surface (implementor-only)
//!
//! `path_begin`, `move_to`, `line_to`, `quad_to`, `cubic_to`, `path_end`,
//! `clip_push`, `clip_pop` are the streaming primitives every backend
//! implements. They take a [`RendererToken`] by value. The token's type is
//! public but its tuple field is private and its constructor is
//! `pub(crate)`, so callers outside the crate cannot construct one and
//! therefore cannot drive the primitives directly — they must go through
//! the safe API above. External `impl Renderer` definitions receive tokens
//! as parameters from default method bodies and treat them as markers.

use std::io::Read;

use crate::scene::{Bitmap, ClipPath, Element, Path, PathStyle, Scene, TextNode, Verb};

/// Proof-of-call witness for the token-gated streaming primitives. The
/// type is `pub` so it can appear in trait method signatures, but its
/// field is private and its constructor is `pub(crate)`. External
/// non-implementor code cannot construct one, so calls like
/// `renderer.clip_pop(...)` cannot originate outside this crate. The
/// default methods on [`Renderer`] mint tokens locally; external
/// implementors receive tokens via their `&mut self` impl bodies and
/// treat them as markers.
#[derive(Clone, Copy)]
pub struct RendererToken(());

impl RendererToken {
    pub(crate) fn new() -> Self {
        Self(())
    }
}

pub trait Renderer {
    /// Called once with the canvas dimensions before any draw command. May be
    /// used by the renderer to allocate output buffers (e.g. a [`tiny_skia::Pixmap`]).
    fn begin(&mut self, width: f32, height: f32);

    /// Called once after the last command. Renderers that buffer output
    /// (PDF, [`tiny_skia::Pixmap`]) flush here.
    fn end(&mut self) {}

    // -------- backend surface (token-gated; implementor-only) --------

    /// Open a new path with the given style. Subsequent `*_to` calls extend
    /// the path; [`Self::path_end`] commits it. Implementors receive the
    /// token by value and treat it as a marker.
    fn path_begin(&mut self, _: RendererToken, style: &PathStyle);
    fn move_to(&mut self, _: RendererToken, x: f32, y: f32);
    fn line_to(&mut self, _: RendererToken, x: f32, y: f32);
    fn quad_to(&mut self, _: RendererToken, cx: f32, cy: f32, x: f32, y: f32);
    #[allow(clippy::too_many_arguments)]
    fn cubic_to(
        &mut self,
        _: RendererToken,
        c1x: f32,
        c1y: f32,
        c2x: f32,
        c2y: f32,
        x: f32,
        y: f32,
    );
    fn path_end(&mut self, _: RendererToken);

    /// Push a clip path onto the clip stack. The path uses the same
    /// verb/coord encoding as a regular path; sub-paths are treated as
    /// implicitly closed, and `clip.fill_rule` decides the inside.
    fn clip_push(&mut self, _: RendererToken, clip: &ClipPath);
    fn clip_pop(&mut self, _: RendererToken);

    // -------- atomic public methods --------

    fn text(&mut self, node: &TextNode);

    /// Blit a previously-uploaded bitmap referenced by `node.id`. The current
    /// renderers (terminal, pdf) skip with a warning; only the future canvas
    /// / WebGL frontends honor this.
    fn bitmap(&mut self, node: &Bitmap);

    // -------- safe high-level API (defaults) --------

    /// Begin a new path styled by `style`. Returns a [`PathScope`] whose
    /// `move_to`/`line_to`/`quad_to`/`cubic_to` methods append to the path;
    /// `path_end` runs on drop. Mirrors [`crate::scene::Scene::path`] but
    /// pushes directly to the renderer.
    ///
    /// Requires `Self: Sized` because of the return type; callers holding a
    /// `&mut dyn Renderer` should use [`Self::render_scene`],
    /// [`Self::render_scene_stream`], or [`Self::draw_path`] with an
    /// already-built [`Path`] instead.
    fn path(&mut self, style: PathStyle) -> PathScope<'_, Self>
    where
        Self: Sized,
    {
        self.path_begin(RendererToken::new(), &style);
        PathScope { renderer: self }
    }

    /// Push a clip onto the clip stack and return a [`ClipScope`]. The scope
    /// derefs to the underlying renderer so commands drawn through it (or
    /// nested clips opened via [`Self::clip`] again) run with the clip
    /// active. `clip_pop` runs on drop.
    ///
    /// Requires `Self: Sized` because of the return type.
    fn clip(&mut self, clip: ClipPath) -> ClipScope<'_, Self>
    where
        Self: Sized,
    {
        self.clip_push(RendererToken::new(), &clip);
        ClipScope { renderer: self }
    }

    /// Replay a complete [`Path`] through the streaming primitives. Walks
    /// `path.verbs`/`path.coords`, calling `path_begin`/`*_to`/`path_end`
    /// with internally-minted tokens. Malformed runs (coords too short for
    /// a verb) stop at the offending verb; `path_end` still runs.
    fn draw_path(&mut self, path: &Path) {
        let tok = RendererToken::new();
        self.path_begin(tok, &path.style);
        let mut i = 0usize;
        for &v in &path.verbs {
            let need = v.coords();
            if path.coords.len() < i + need {
                break;
            }
            let c = &path.coords[i..];
            match v {
                Verb::Move => self.move_to(tok, c[0], c[1]),
                Verb::Line => self.line_to(tok, c[0], c[1]),
                Verb::Quad => self.quad_to(tok, c[0], c[1], c[2], c[3]),
                Verb::Cubic => self.cubic_to(tok, c[0], c[1], c[2], c[3], c[4], c[5]),
            }
            i += need;
        }
        self.path_end(tok);
    }

    /// Walk a slice of [`Element`]s, dispatching each one through the trait.
    /// `Clipped` subtrees recurse — the balanced `clip_push`/`clip_pop` pair
    /// is emitted around the nested walk.
    fn render_elements(&mut self, elements: &[Element]) {
        let tok = RendererToken::new();
        for node in elements {
            match node {
                Element::Path(p) => self.draw_path(p),
                Element::Clipped { clip, elements } => {
                    self.clip_push(tok, clip);
                    self.render_elements(elements);
                    self.clip_pop(tok);
                }
                Element::Text(t) => self.text(t),
                Element::Bitmap(b) => self.bitmap(b),
            }
        }
    }

    /// Replay a complete [`Scene`] in a single `begin`/`end` envelope. The
    /// canonical entry point — [`Scene::render`](crate::scene::Scene::render)
    /// is a thin alias.
    fn render_scene(&mut self, scene: &Scene) {
        self.begin(scene.width, scene.height);
        self.render_elements(&scene.elements);
        self.end();
    }

    /// Decode one Cap'n Proto `Frame` message from `reader` and dispatch
    /// it through the renderer's streaming primitives. The Cap'n Proto
    /// reader is walked lazily — `Element` lists never materialize into
    /// a `Vec<Element>`, and `Clipped` subtrees recurse via
    /// `clip_push`/`clip_pop` rather than a buffered subtree. Per-path
    /// `PathStyle` is still materialized (bounded by one path).
    ///
    /// Expects exactly one `Message::Frame` payload; other variants
    /// (`Asset`, `Event`, `SessionClose`) return [`wire::Error::WrongMessageKind`].
    /// Callers with mixed message streams should peek the kind themselves
    /// or use [`wire::decode`].
    ///
    /// Requires `Self: Sized`; thin delegate to
    /// [`crate::wire::render_scene_stream`].
    fn render_scene_stream<R: Read>(&mut self, reader: R) -> Result<(), crate::wire::Error>
    where
        Self: Sized,
    {
        crate::wire::render_scene_stream(self, reader)
    }
}

/// Active path scope returned by [`Renderer::path`]. Calls
/// `move_to`/`line_to`/`quad_to`/`cubic_to` on the underlying renderer;
/// runs `path_end` on drop. Tokens are minted locally — external callers
/// drive the primitives only through this type and through [`ClipScope`].
#[must_use = "PathScope commits path_end on drop; bind it so the geometry methods can run"]
pub struct PathScope<'a, R: Renderer + ?Sized> {
    renderer: &'a mut R,
}

impl<'a, R: Renderer + ?Sized> PathScope<'a, R> {
    pub fn move_to(&mut self, x: f32, y: f32) -> &mut Self {
        self.renderer.move_to(RendererToken::new(), x, y);
        self
    }

    pub fn line_to(&mut self, x: f32, y: f32) -> &mut Self {
        self.renderer.line_to(RendererToken::new(), x, y);
        self
    }

    pub fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) -> &mut Self {
        self.renderer.quad_to(RendererToken::new(), cx, cy, x, y);
        self
    }

    pub fn cubic_to(
        &mut self,
        c1x: f32,
        c1y: f32,
        c2x: f32,
        c2y: f32,
        x: f32,
        y: f32,
    ) -> &mut Self {
        self.renderer
            .cubic_to(RendererToken::new(), c1x, c1y, c2x, c2y, x, y);
        self
    }
}

impl<'a, R: Renderer + ?Sized> Drop for PathScope<'a, R> {
    fn drop(&mut self) {
        self.renderer.path_end(RendererToken::new());
    }
}

/// Active clip scope returned by [`Renderer::clip`]. Derefs to the
/// underlying renderer so commands drawn through the scope run with the
/// clip active. Calls `clip_pop` on drop.
#[must_use = "ClipScope commits clip_pop on drop; bind it where the clip should end"]
pub struct ClipScope<'a, R: Renderer + ?Sized> {
    renderer: &'a mut R,
}

impl<'a, R: Renderer + ?Sized> std::ops::Deref for ClipScope<'a, R> {
    type Target = R;
    fn deref(&self) -> &R {
        &*self.renderer
    }
}

impl<'a, R: Renderer + ?Sized> std::ops::DerefMut for ClipScope<'a, R> {
    fn deref_mut(&mut self) -> &mut R {
        &mut *self.renderer
    }
}

impl<'a, R: Renderer + ?Sized> Drop for ClipScope<'a, R> {
    fn drop(&mut self) {
        self.renderer.clip_pop(RendererToken::new());
    }
}

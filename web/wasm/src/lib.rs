//! The screen of the HTML client in Rust. It reads the messages of the
//! server with [`FrameReader`] and draws each frame with
//! [`CanvasRenderer`], so the page in TypeScript only opens the WebSocket
//! and sends the input. `web/src/rust.ts` loads it.

use sinteract::renderer::Renderer;
use sinteract::renderer::canvas::CanvasRenderer;
use sinteract::scene::Scene;
use sinteract::view::FrameReader;
use wasm_bindgen::prelude::*;
use web_sys::HtmlCanvasElement;

/// The frames of one connection on a canvas.
#[wasm_bindgen]
pub struct Screen {
    reader: FrameReader,
    renderer: CanvasRenderer,
    /// Called when the images of `latest` are ready.
    redraw: js_sys::Function,
    /// The frame on the canvas.
    scene: Option<Scene>,
    /// The newest frame, which goes on the canvas once the browser decoded
    /// its images, unless a newer frame comes first.
    latest: Option<Scene>,
}

#[wasm_bindgen]
impl Screen {
    #[wasm_bindgen(constructor)]
    pub fn new(canvas: HtmlCanvasElement, redraw: js_sys::Function) -> Screen {
        Screen {
            reader: FrameReader::new(),
            renderer: CanvasRenderer::new(canvas),
            redraw,
            scene: None,
            latest: None,
        }
    }

    /// Read `payload`, a message of the server. Returns `true` if it was a
    /// frame, which the next `draw` draws, `false` otherwise.
    pub fn read(&mut self, payload: &[u8]) -> Result<bool, JsError> {
        let Some(scene) = self.reader.read(payload)? else {
            return Ok(false);
        };
        self.latest = Some(scene);
        Ok(true)
    }

    /// Draw the newest frame whose images are ready, fit to the size of the
    /// canvas. While the images of a newer frame decode, the frame before
    /// it stays, and `redraw` runs when they are ready.
    pub fn draw(&mut self) {
        if let Some(latest) = &self.latest {
            match self.renderer.load(latest) {
                None => self.scene = self.latest.take(),
                Some(ready) => _ = ready.unchecked_ref::<Thenable>().then_call(&self.redraw),
            }
        }
        if let Some(scene) = &self.scene {
            let Ok(()) = self.renderer.render(scene);
        }
    }

    /// Clear the canvas.
    pub fn clear(&self) {
        self.renderer.clear();
    }

    /// The scale of the last frame on the canvas.
    #[wasm_bindgen(getter)]
    pub fn scale(&self) -> f32 {
        self.renderer.place().scale
    }

    /// The left of the last frame on the canvas, in pixels of the canvas.
    #[wasm_bindgen(getter)]
    pub fn x(&self) -> f32 {
        self.renderer.place().x
    }

    /// The top of the last frame on the canvas, in pixels of the canvas.
    #[wasm_bindgen(getter)]
    pub fn y(&self) -> f32 {
        self.renderer.place().y
    }
}

#[wasm_bindgen]
extern "C" {
    /// A promise seen through `then` with a plain function, which
    /// [`js_sys::Promise::then`] does not take.
    type Thenable;

    #[wasm_bindgen(method, js_name = then)]
    fn then_call(this: &Thenable, f: &js_sys::Function) -> js_sys::Promise;
}

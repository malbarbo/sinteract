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
    scene: Option<Scene>,
}

#[wasm_bindgen]
impl Screen {
    #[wasm_bindgen(constructor)]
    pub fn new(canvas: HtmlCanvasElement) -> Screen {
        Screen {
            reader: FrameReader::new(),
            renderer: CanvasRenderer::new(canvas),
            scene: None,
        }
    }

    /// Read `payload`, a message of the server. Returns `true` if it was a
    /// frame, which the next `draw` draws, `false` otherwise.
    pub fn read(&mut self, payload: &[u8]) -> Result<bool, JsError> {
        let Some(scene) = self.reader.read(payload)? else {
            return Ok(false);
        };
        self.scene = Some(scene);
        Ok(true)
    }

    /// Draw the last frame, fit to the size of the canvas.
    pub fn draw(&mut self) {
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

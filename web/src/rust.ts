// The page of a player that reads and draws the frames in Rust, with the
// Screen of wasm/. The module of wasm/ comes in the bundle, so the page is
// still one file.

import { runPage } from "./page.ts";
import { initSync, Screen } from "../build/wasm/sinteract.js";
import wasm from "../build/wasm/sinteract_bg.ts";

initSync({ module: Uint8Array.fromBase64(wasm) });
runPage((canvas) => new Screen(canvas));

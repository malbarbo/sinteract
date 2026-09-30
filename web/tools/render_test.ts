// Draws the gallery with the client in a headless Chrome and compares it
// with the PNG of the pixmap renderer, tile by tile. The text and the
// antialiasing of Chrome never match tiny-skia pixel by pixel, so a tile
// fails only when its mean difference passes TOLERANCE.
//
//   deno run -A tools/render_test.ts CHROME SERVER PAGE GALLERY EXPECTED ACTUAL
//
// SERVER is the binary of web/server, PAGE the page of the client that it
// serves, GALLERY the binary of the gallery example, EXPECTED its PNG at
// scale 2, and ACTUAL the PNG that receives the screenshot of Chrome.

import { decode, type DecodedPng } from "fast-png";
import puppeteer from "puppeteer-core";

// The side of a tile, in CSS pixels.
const TILE = 50;
// The largest mean difference of a tile, per channel, out of 255.
const TOLERANCE = 6;
// How long the screenshots wait for a frame that passes.
const TIMEOUT_MS = 10_000;
// The scale of the PNG of the gallery example.
const SCALE = 2;

const [chrome, server, pagePath, gallery, expectedPath, actualPath] = Deno.args;
if (
  !chrome || !server || !pagePath || !gallery || !expectedPath || !actualPath
) {
  console.error(
    "usage: render_test.ts CHROME SERVER PAGE GALLERY EXPECTED ACTUAL",
  );
  Deno.exit(1);
}

const expected = decode(await Deno.readFile(expectedPath));
const width = expected.width / SCALE;
const height = expected.height / SCALE;

const child = new Deno.Command(server, {
  args: [
    "--addr",
    "127.0.0.1:0",
    "--players",
    "1",
    "--page",
    pagePath,
    gallery,
    "stage",
  ],
  stdout: "null",
  stderr: "piped",
}).spawn();
const browser = await puppeteer.launch({ executablePath: chrome });
let failures: string[];
try {
  const url = await linkOf(child.stderr);
  const page = await browser.newPage();
  page.on("pageerror", (e) => console.error(`page: ${e}`));
  await page.setViewport({ width, height, deviceScaleFactor: SCALE });
  await page.goto(url);
  // The page draws its first frame some time after the load, so the test
  // takes screenshots until one passes or the time runs out.
  const end = Date.now() + TIMEOUT_MS;
  let shot: Uint8Array;
  do {
    await new Promise((resolve) => setTimeout(resolve, 250));
    shot = await page.screenshot({ type: "png" });
    failures = compare(expected, decode(shot));
  } while (failures.length > 0 && Date.now() < end);
  await Deno.writeFile(actualPath, shot);
} finally {
  await browser.close();
  child.kill();
  await child.status;
}

if (failures.length > 0) {
  console.error(`${failures.length} tiles differ, see ${actualPath}:`);
  for (const failure of failures) console.error(`  ${failure}`);
  Deno.exit(1);
}
console.log(`${pagePath} draws the gallery as the pixmap does`);

// The link of the first player, from the lines that the server prints. The
// rest of the stream still goes to the terminal.
async function linkOf(stream: ReadableStream<Uint8Array>): Promise<string> {
  const reader = stream.getReader();
  const decoder = new TextDecoder();
  let text = "";
  for (;;) {
    const { value, done } = await reader.read();
    if (done) throw new Error(`the server ended with no link:\n${text}`);
    text += decoder.decode(value, { stream: true });
    const link = text.match(/http:\/\/\S+/)?.[0];
    if (link) {
      forward(reader);
      return link;
    }
  }
}

async function forward(reader: ReadableStreamDefaultReader<Uint8Array>) {
  for (;;) {
    const { value, done } = await reader.read();
    if (done) return;
    await Deno.stderr.write(value);
  }
}

// Each tile of `actual` whose mean difference from `expected` passes
// TOLERANCE, with its place and its difference.
function compare(expected: DecodedPng, actual: DecodedPng): string[] {
  if (actual.width !== expected.width || actual.height !== expected.height) {
    return [
      `the screenshot is ${actual.width}x${actual.height}, ` +
      `and the PNG ${expected.width}x${expected.height}`,
    ];
  }
  const side = TILE * SCALE;
  const failures = [];
  for (let y0 = 0; y0 < expected.height; y0 += side) {
    for (let x0 = 0; x0 < expected.width; x0 += side) {
      let sum = 0;
      let count = 0;
      for (let y = y0; y < Math.min(y0 + side, expected.height); y++) {
        for (let x = x0; x < Math.min(x0 + side, expected.width); x++) {
          for (let c = 0; c < 3; c++) {
            sum += Math.abs(
              channel(expected, x, y, c) - channel(actual, x, y, c),
            );
            count++;
          }
        }
      }
      const mean = sum / count;
      if (mean > TOLERANCE) {
        failures.push(
          `(${x0 / SCALE}, ${y0 / SCALE}) differs by ${mean.toFixed(1)}`,
        );
      }
    }
  }
  return failures;
}

// Channel `c` of the pixel at (`x`, `y`) of `png`, from 0 to 255.
function channel(png: DecodedPng, x: number, y: number, c: number): number {
  const i = (y * png.width + x) * png.channels + c;
  return png.depth === 16 ? png.data[i] >> 8 : png.data[i];
}

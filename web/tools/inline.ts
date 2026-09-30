// Puts the bundle of the page into the page, so the client is one HTML file
// with no other request.
//
//   deno run --allow-read --allow-write tools/inline.ts build/app.js \
//     src/index.html dist/index.html

const [bundlePath, htmlPath, outputPath] = Deno.args;
if (!bundlePath || !htmlPath || !outputPath) {
  console.error("usage: inline.ts BUNDLE HTML OUTPUT");
  Deno.exit(1);
}

const html = await Deno.readTextFile(htmlPath);
const bundle = await Deno.readTextFile(bundlePath);
const tag = '<script type="module" src="app.js"></script>';
// A tag that is not there would leave a page that asks the network for an
// app.js that is not beside it.
if (!html.includes(tag)) {
  console.error(`inline.ts: ${htmlPath} has no ${tag}`);
  Deno.exit(1);
}
// A function, so a "$&" in the bundle is text like any other.
const script = `<script type="module">${
  bundle.replaceAll("</script", "<\\/script")
}</script>`;
const output = html.replace(tag, () => script);
await Deno.writeTextFile(outputPath, output);
console.log(`${outputPath}: ${(output.length / 1024).toFixed(1)} KB`);

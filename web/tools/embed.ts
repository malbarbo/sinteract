// Writes a module whose default export is the base64 of a file, so the
// bundle of a page carries the module of wasm/ and the page stays one file.
//
//   deno run --allow-read --allow-write tools/embed.ts INPUT OUTPUT

const [inputPath, outputPath] = Deno.args;
if (!inputPath || !outputPath) {
  console.error("usage: embed.ts INPUT OUTPUT");
  Deno.exit(1);
}

const bytes = await Deno.readFile(inputPath);
await Deno.writeTextFile(
  outputPath,
  `export default "${bytes.toBase64()}";\n`,
);

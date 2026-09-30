// Generates src/capnp/*.ts from the files in ../schema with capnp-es. The
// capnp CLI parses the schema and writes the request of a plugin, which this
// script reads on stdin:
//
//   capnp compile -o- --src-prefix=../schema ../schema/scene.capnp \
//     ../schema/event.capnp ../schema/protocol.capnp \
//     | deno run --allow-write=src/capnp tools/capnp.ts
//
// `make capnp` runs the same command. The generated files are committed, so
// the build does not need the capnp CLI. Do not edit them by hand.

import { Buffer } from "node:buffer";
import { compileAll } from "capnp-es/compiler";
// capnp-es takes typescript as an optional peer, and the compiler needs it.
import "typescript";

const request = new Uint8Array(
  await new Response(Deno.stdin.readable).arrayBuffer(),
);
const { files } = await compileAll(Buffer.from(request), { ts: true });
for (const [path, source] of files) {
  const name = path.split("/").pop();
  // Deno resolves an import by its real extension.
  const fixed = source.replace(/from "\.\/(\w+)\.js"/g, 'from "./$1.ts"');
  await Deno.writeTextFile(`src/capnp/${name}`, fixed);
  console.error(`src/capnp/${name}`);
}

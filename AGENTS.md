# sinteract

A 2D graphics library for spython and sgleam. A front end builds a `Scene`,
a draw list of paths, text, bitmaps and clipped subtrees, and a renderer
replays it. `scene.rs` and `event.rs` hold the value types and the
builders, and `text.rs` resolves a font, measures it and outlines a glyph.

`renderer/` holds the `Renderer` trait in `mod.rs` and the three backends,
`pixmap.rs` for tiny-skia, `pdf.rs` for vector PDF and `svg.rs` for SVG.

`wire/` is the Cap'n Proto codec, in three layers: `wire/scene.rs` and
`wire/event.rs` convert the values of `scene.rs` and `event.rs` to and from
the schema structs and know nothing of a session, `wire/protocol.rs` owns
the `Message` envelope, and `wire/framing.rs` owns the envelope that a byte
stream needs.

`display/` shows a scene and reads the input back. `driver.rs` holds the
sealed `Display` trait, `inbox.rs` the queue and the `Sender` behind
`wait_event`, `terminal.rs` and `window.rs` are the two displays, and
`sixel.rs` and `term_query.rs` support them.
Only `sixel.rs` builds on wasm32, so the `cfg` sits on each submodule in
`display/mod.rs` and not on the whole directory.

`PLAN.md` is the plan for the server and client modes.

## Build and test

```sh
cargo fmt
cargo clippy --all-targets
cargo clippy --all-targets --no-default-features   # without the displays
cargo test
cargo build --target wasm32-unknown-unknown   # the part a browser client uses
cargo clippy --all-targets --target wasm32-unknown-unknown
cargo doc --no-deps                           # warning-free
cargo doc --no-deps --target wasm32-unknown-unknown
```

Tests live next to the code they test, in `#[cfg(test)]` modules. Run
`cargo fmt`, `cargo clippy --all-targets` and `cargo test` before you call a
change done. Clippy and rustdoc print no warnings. Do not leave files in the
root of the repository.

## Scope

Change what was asked and nothing else. A problem you see on the way is
something to report, not to fix in the same change. Before you start, write
the subject of the commit. If it does not fit in one concrete line, the
change is more than one, and it becomes more than one commit.

## Conventions in the code

- Make an invalid state impossible to build. When two fields have to agree,
  the type enforces it, with private fields and a constructor that checks,
  or with a representation that has no invalid case. A scope that has to be
  closed is an RAII guard.
- The `Renderer` trait stays small. `Scene` owns the format, expands an arc
  to cubics, and elevates a quadratic for a backend that has no quadratic
  operator, so a backend only walks segments.
- `schema/` is the source of truth for the wire format, one file per
  layer. `scene.capnp` is the drawing vocabulary, `event.capnp` the input,
  and `protocol.capnp` the session that carries both. Evolve a file by
  appending fields with defaults. Never reorder or renumber, and let a
  union only grow. A reader skips an element, an event or a message of an
  arm it does not know, and an element or an event that holds a value it
  does not know, such as an enum value or a verb byte. A paint of an arm it
  does not know draws its fallback color, so a writer of a new paint arm
  sets `fallback` and `hasFallback`. Damage is still an error.
- `src/wire/*_capnp.rs` are generated and committed, so the build does not
  need the `capnp` CLI. Regenerate them with the command at the top of
  `schema/scene.capnp`. Do not edit them by hand.
- A `#[repr(u8)]` enum that crosses the wire, such as `SegmentKind` or
  `LineCap`, has the discriminants of the schema.
- A native-only module is gated once, where its parent declares it, behind
  `cfg(not(target_arch = "wasm32"))`. No `cfg(wasm)` inside a module body.

# Writing

These rules cover every piece of English in the repository: comments, doc
comments, commit messages, and the README.

## Comments

Delete first. Most comments should not exist. The name, the type, and the
line below already say it. Ask what the comment adds before you ask how it
reads.

- Say why, not what. The reason for an assert, the case a branch guards, the
  invariant a caller has to keep.
- If the reader can see it in the line below, drop the comment.
- Never comment history. What the code used to do, a bug that was fixed, an
  API that is kept "for the historic callers", none of that belongs in a
  comment about the code as it stands.
- Put the comment on the thing it describes.
- Check that the comment is true in every case the code covers.
- Keep it short. One or two lines is the norm. A paragraph needs a reason.
- A function that answers yes or no says so first: `Returns \`true\` if ...,
  \`false\` otherwise.` The details come after.

## How a sentence reads

Plain, active English. Subject, verb, object. The reader takes it in one word
at a time, left to right, without going back.

Avoid the constructions that make the reader do the grammar:

- **Passive voice.** It hides who acts.
  - No: `Sub-paths are treated as implicitly closed.`
  - Yes: `A sub-path closes implicitly.`
- **A preposition left at the end of a relative clause.** The object of the
  preposition sits at the head of the clause, so the reader only finds out at
  the end what to fill the gap with. Portuguese has no such construction,
  which makes it doubly hard to read.
  - No: `the mask the clip shape is rasterized into`
  - Yes: `the mask that receives the clip shape`
- **An elided verb.** Write the verb out.
- **A pronoun for a noun that is far away.** Name the noun again.
- **A clause that ends in its verb.** The reader holds the noun until the
  verb arrives, which is the same trouble as a stranded preposition.
  - No: `the scale the caller decided`
  - Yes: `the scale from the caller`
- **A colon in the middle of a sentence.** It joins two halves and asks the
  reader to hold the first one. Write two sentences. A colon before a list
  or after a label is fine.
  - No: `The gradient is boxed: the common case is a solid color.`
  - Yes: `The gradient is boxed. The common case is a solid color.`
- **A dash in the middle of a sentence.** Same trouble as the colon. Write
  two sentences, or use a comma.
  - No: `Empty family resolves to Liberation Sans — keeps the historic render`
  - Yes: `An empty family resolves to Liberation Sans.`
- **A metaphor for something the code states plainly.**
  - No: `it cost what invariants cost`, `a band-aid`, `closes that vector`
  - Yes: `it needed four accessors and a validating constructor`
- **A word that sounds clever.** `owe`, `pay for`, `buy`, `travel`, `land`,
  `reach out`, `lie`. Say `cost`, `go`, `is`, `call`, `is wrong`.

## Commit messages

One change per commit. A refactor and the fix it prepares are two commits.
A commit that unifies two gradient types and also boxes the paint is two
commits.

The subject says what the change does to the code, in one line:

- A prefix for the module, then a lowercase sentence: `scene:`, `renderer:`,
  `wire:`, `pixmap:`, `pdf:`, `terminal:`, `window:`, `text:`,
  `display:`, `docs:`, and `all:` when it crosses them.
- Imperative or present, active voice. `pixmap: keep a pool of clip masks`.
- Name the concrete thing that changed and what happened to it. A type, a
  function, a field, a message. The reader should know what to look for in
  the diff.
  - No: `Measure text once per node, not three times`
  - Yes: `text: fold the measure helpers into ResolvedFont::measure`
  - No: `Recycle clip masks instead of allocating two per node`
  - Yes: `pixmap: keep a pool of clip masks`
- No rhetorical contrast, no metaphor, no joke. `not just`, `instead of`,
  `rather than` and `only` in a subject usually mean the subject is
  describing the old code instead of the new one.
- Under about 65 characters. No period at the end.

The body is optional. Write one when the subject leaves something out, which
is usually why:

- What was wrong, then what the change does. Two to four sentences, one
  paragraph, wrapped at 72 columns. A size that changed goes in a sentence,
  not in a `Measured:` line.
- No bullet lists, no list of files, no narration of the process (`also`,
  `while at it`, `along the way`, `fmt/clippy clean`). What is not worth a
  sentence is in the diff.
- No trailers. The message ends at the last line of the body. No
  `Co-Authored-By`, no `Claude-Session`, no `Signed-off-by`.

A good one:

```
pixmap: keep a pool of clip masks

Every clipped element allocated two canvas-sized masks and rasterized
the canvas twice, once for the parent and once for the clip shape. The
clip shape now goes into a pooled mask, multiplied in place by the
parent, and the mask returns to the pool when the scope ends. A mask
from the pool is cleared before use, because fill_path adds coverage.
```

## The README and PLAN.md

Prose, not lists of features. Show a thing where it is used, once. No bold
label followed by a dash. PLAN.md is in Portuguese and stays so.

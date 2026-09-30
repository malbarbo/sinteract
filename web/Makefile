# The HTML client of a view. `make` builds the two pages of the client,
# each one file. dist/index.html reads and draws the frames in TypeScript,
# and dist/rust.html in Rust, with the crate of wasm/, which wasm-pack
# builds. `make check` checks the format and the types, and `make test`
# runs the tests with Deno. `make render-test` draws the gallery with each
# page in Chrome and compares it with the PNG of the pixmap renderer.

SRC_TS = $(sort $(shell find src -name '*.ts'))
SRC_RS = $(sort $(shell find wasm/src ../src -name '*.rs')) wasm/Cargo.toml \
	../Cargo.toml $(wildcard ../fonts/*.ttf)
SCHEMA = ../schema/scene.capnp ../schema/event.capnp ../schema/protocol.capnp
CHROME ?= $(firstword $(shell command -v chromium google-chrome))

all: dist/index.html dist/rust.html

dist/%.html: build/%.js src/index.html tools/inline.ts
	@mkdir -p $(@D)
	deno run --allow-read --allow-write tools/inline.ts $< src/index.html $@

build/index.js: $(SRC_TS) deno.json
	deno bundle --platform browser --minify src/main.ts -o $@

build/rust.js: $(SRC_TS) deno.json build/wasm/sinteract_bg.ts
	deno bundle --platform browser --minify src/rust.ts -o $@

build/wasm/sinteract.js: $(SRC_RS)
	wasm-pack build --release --target web --no-pack \
		--out-dir ../build/wasm --out-name sinteract wasm

build/wasm/sinteract_bg.ts: build/wasm/sinteract.js tools/embed.ts
	deno run --allow-read --allow-write tools/embed.ts \
		build/wasm/sinteract_bg.wasm $@

# Needs the capnp CLI. The generated files are committed.
capnp: $(SCHEMA) tools/capnp.ts
	capnp compile -o- --src-prefix=../schema $(SCHEMA) \
		| deno run --allow-read --allow-env --allow-write=src/capnp tools/capnp.ts

# rust.ts imports the module that wasm-pack writes.
check: build/wasm/sinteract_bg.ts
	deno fmt --check
	deno lint
	deno check src/ tests/ tools/

test:
	deno test --allow-read tests/

render-test: dist/index.html dist/rust.html
	cargo build --release --example gallery --manifest-path ../Cargo.toml
	cargo build --release --manifest-path server/Cargo.toml
	../target/release/examples/gallery png > build/gallery.png
	for page in index rust; do \
		deno run -A tools/render_test.ts $(CHROME) \
			server/target/release/sinteract-test-server dist/$$page.html \
			../target/release/examples/gallery build/gallery.png \
			build/gallery-$$page.png || exit 1; \
	done

clean:
	rm -rf build dist

.PHONY: all capnp check test render-test clean

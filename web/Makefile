# The HTML client of a view. `make` builds dist/index.html, one file with
# the whole client. `make check` checks the format and the types, and
# `make test` runs the tests with Deno. `make render-test` draws the gallery
# in Chrome and compares it with the PNG of the pixmap renderer.

SRC_TS = $(sort $(shell find src -name '*.ts'))
SCHEMA = ../schema/scene.capnp ../schema/event.capnp ../schema/protocol.capnp
CHROME ?= $(firstword $(shell command -v chromium google-chrome))

dist/index.html: build/app.js src/index.html tools/inline.ts
	@mkdir -p $(@D)
	deno run --allow-read --allow-write tools/inline.ts build/app.js src/index.html $@

build/app.js: $(SRC_TS) deno.json
	deno bundle --platform browser --minify src/main.ts -o $@

# Needs the capnp CLI. The generated files are committed.
capnp: $(SCHEMA) tools/capnp.ts
	capnp compile -o- --src-prefix=../schema $(SCHEMA) \
		| deno run --allow-read --allow-env --allow-write=src/capnp tools/capnp.ts

check:
	deno fmt --check
	deno lint
	deno check src/ tests/ tools/

test:
	deno test --allow-read tests/

render-test: dist/index.html
	cargo build --release --example gallery --manifest-path ../Cargo.toml
	cargo build --release --manifest-path server/Cargo.toml
	../target/release/examples/gallery png > build/gallery.png
	deno run -A tools/render_test.ts $(CHROME) \
		server/target/release/sinteract-test-server \
		../target/release/examples/gallery build/gallery.png build/gallery-web.png

clean:
	rm -rf build dist

.PHONY: capnp check test render-test clean

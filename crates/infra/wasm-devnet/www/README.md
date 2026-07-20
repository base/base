# base-wasm-devnet — Browser UI

A static, plain HTML/CSS/JS web app (no bundler) that drives the in-browser
Base devnet compiled to WebAssembly.

## Build & run

```sh
./build.sh
```

This runs `wasm-pack build --target web --no-opt` into `www/pkg/` (git-ignored,
regenerated on every build) and starts a local server on `http://localhost:8765`
with the `Cross-Origin-Opener-Policy: same-origin` and
`Cross-Origin-Embedder-Policy: require-corp` headers required for
`SharedArrayBuffer` (the wasm32 target is built with `+atomics`).

## Files

- `index.html` — the app
- `pkg/` — wasm-bindgen output (git-ignored, run `build.sh` to generate)
- `serve.py` — COOP/COEP-enabled static file server

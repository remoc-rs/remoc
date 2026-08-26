# Web RTC counter example

This example uses Remoc RTC from a web browser. The server keeps one counter
shared by all connected clients. Each browser can increment or decrement it and
receives changes through a Remoc watch channel.

It is split into three crates:

* `counter` defines the remote trait shared by client and server.
* `counter-client-web` connects through the browser WebSocket API and exposes
  the RTC client to JavaScript with `wasm-bindgen`.
* `counter-server-web` serves the web page and Remoc WebSocket endpoint from one
  Axum server.

No JavaScript package manager or bundler is required.

## Prerequisites

Install the WebAssembly target and `wasm-pack`:

```console
rustup target add wasm32-unknown-unknown
cargo install wasm-pack
```

`wasm-pack` selects a `wasm-bindgen` CLI version compatible with the Rust
dependency resolved for the client.

## Running

From the top-level repository directory, run:

```console
./examples/rtc-web/run.sh
```

Then open <http://127.0.0.1:9872> in two browser windows. Changes made in either
window are shown in both.

The script first builds the browser client and runs `wasm-bindgen`. It then
compiles the server with the generated JavaScript and WebAssembly embedded in
the executable. Generated files are placed below `examples/rtc-web/target/`.
The workspace release profile enables size optimization and link-time
optimization for the WebAssembly module.

The same steps can be run without the shell script, including on Windows:

```console
wasm-pack build examples/rtc-web/counter-client-web --target web --release --no-pack --no-typescript --out-name counter_web --out-dir ../target/web
cargo run --manifest-path examples/rtc-web/Cargo.toml --package counter-server-web
```

To build without starting the server, use:

```console
./examples/rtc-web/build.sh
```

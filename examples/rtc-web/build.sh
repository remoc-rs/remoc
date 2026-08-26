#!/usr/bin/env bash
set -euo pipefail

example_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
assets_dir="${example_dir}/target/web"

# Compile the Rust browser client and generate the JavaScript loader for it.
wasm-pack build \
    "${example_dir}/counter-client-web" \
    --target web \
    --release \
    --no-pack \
    --no-typescript \
    --out-name counter_web \
    --out-dir "${assets_dir}"

# Compile the native server, which embeds the generated browser files.
cargo build \
    --manifest-path "${example_dir}/Cargo.toml" \
    --package counter-server-web

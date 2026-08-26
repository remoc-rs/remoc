#!/usr/bin/env bash
set -euo pipefail

example_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

"${example_dir}/build.sh"

cargo run \
    --manifest-path "${example_dir}/Cargo.toml" \
    --package counter-server-web

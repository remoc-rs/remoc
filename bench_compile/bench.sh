#!/usr/bin/env bash
#
# Compile-time benchmark for remoc channels.
#
# Measures how long it takes to compile a binary that creates
# mpsc, oneshot, and watch channels for 20+ complex nested types,
# forcing monomorphization of remoc channel infrastructure.
#
# Usage:
#   ./bench.sh                  # run release benchmark
#   ./bench.sh --debug          # run debug benchmark
#   ./bench.sh --both           # run both debug and release

set -euo pipefail
cd "$(dirname "$0")"

PROFILE="release"
PROFILE_FLAG="--release"
RUN_BOTH=false

for arg in "$@"; do
    case "$arg" in
        --debug)
            PROFILE="debug"
            PROFILE_FLAG=""
            ;;
        --both)
            RUN_BOTH=true
            ;;
    esac
done

bench() {
    local profile="$1"
    local profile_flag="$2"

    echo "=== Building remoc compile benchmark ($profile) ==="

    # Ensure dependencies are pre-built.
    cargo build $profile_flag --bin remoc_channels 2>/dev/null

    # Touch the binary source to force recompilation of only that crate.
    touch "src/remoc_channels.rs"

    # Time the rebuild.
    start=$(date +%s%N)
    cargo build $profile_flag --bin remoc_channels 2>/dev/null
    end=$(date +%s%N)
    elapsed=$(echo "scale=2; ($end - $start) / 1000000000" | bc)

    # Binary size.
    bin="target/${profile}/remoc_channels"
    if [[ -f "$bin" ]]; then
        size_kb=$(( $(stat -c%s "$bin") / 1024 ))
    else
        size_kb="-"
    fi

    printf "%-30s %8s s  %8s KB\n" "remoc_channels ($profile)" "$elapsed" "$size_kb"

    # Run to verify correctness.
    "$bin"
}

echo "Pre-building dependencies..."
cargo build --release --bin remoc_channels 2>/dev/null
if [[ "$RUN_BOTH" == true ]] || [[ "$PROFILE" == "debug" ]]; then
    cargo build --bin remoc_channels 2>/dev/null
fi

echo ""
printf "%-30s %8s     %8s\n" "Target" "Time" "Size"
printf "%-30s %8s     %8s\n" "------" "------" "------"

if [[ "$RUN_BOTH" == true ]]; then
    bench "debug" ""
    bench "release" "--release"
else
    bench "$PROFILE" "$PROFILE_FLAG"
fi

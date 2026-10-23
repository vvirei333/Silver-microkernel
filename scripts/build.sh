#!/usr/bin/env bash
set -euo pipefail

target="${1:-all}"
profile="${2:-dev}"

case "$target" in
    host|kernel|all) ;;
    *)
        echo "Usage: $0 [host|kernel|all] [dev|release]" >&2
        exit 2
        ;;
esac

case "$profile" in
    dev) cargo_args=(build) ;;
    release) cargo_args=(build --release) ;;
    *)
        echo "Usage: $0 [host|kernel|all] [dev|release]" >&2
        exit 2
        ;;
esac

if [[ "$target" == "host" || "$target" == "all" ]]; then
    cargo "${cargo_args[@]}"
fi

if [[ "$target" == "kernel" || "$target" == "all" ]]; then
    cargo "${cargo_args[@]}" --target aarch64-unknown-none \
        --no-default-features --features no_std
fi

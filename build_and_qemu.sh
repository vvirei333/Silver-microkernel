#!/usr/bin/env bash
# Builds the kernel together with the embedded userspace ELFs (build.rs
# re-invokes cargo inside userspace/) and boots the result in QEMU with
# disk.img attached as a virtio-blk-pci device, which sfs_server needs, plus a
# ramfb framebuffer on a GTK window for the graphics driver. The UART shell
# stays multiplexed with the QEMU monitor on stdio.
#
# Usage: ./build_and_qemu.sh [debug|release] [--no-run]
#   profile defaults to release (the handoff's recommended profile for SFS
#   work); --no-run stops after the build.
set -euo pipefail

cd "$(dirname "$0")"

# rustup's cargo may sit outside the minimal PATH an IDE shell exports, so the
# toolchain bin directory is prepended when cargo is not already reachable.
if ! command -v cargo >/dev/null 2>&1; then
    for candidate in "$HOME/.cargo/bin" "$HOME"/.rustup/toolchains/*/bin; do
        if [ -x "$candidate/cargo" ]; then
            PATH="$candidate:$PATH"
            break
        fi
    done
fi

PROFILE=release
RUN=1
for argument in "$@"; do
    case "$argument" in
        debug | release) PROFILE="$argument" ;;
        --no-run) RUN=0 ;;
        *) echo "usage: $0 [debug|release] [--no-run]" >&2; exit 1 ;;
    esac
done

cargo build "--$PROFILE" --target aarch64-unknown-none \
    --no-default-features --features no_std

if [ "$RUN" -ne 1 ]; then
    echo "built: target/aarch64-unknown-none/$PROFILE/silver_microkernel"
    exit 0
fi

exec qemu-system-aarch64 \
    -machine virt -cpu cortex-a72 -m 128M \
    -kernel "target/aarch64-unknown-none/$PROFILE/silver_microkernel" \
    -drive file=disk.img,format=raw,if=none,id=hd \
    -device virtio-blk-pci,drive=hd \
    -device ramfb \
    -device virtio-tablet-pci,disable-legacy=on,id=tablet0 \
    -display gtk,show-cursor=on \
    -serial mon:stdio

#!/usr/bin/env bash
# Builds the kernel together with the embedded userspace ELFs (build.rs
# re-invokes cargo inside userspace/) and boots the result in QEMU with
# disk.img attached as a virtio-blk-pci device (the kernel SFS), a virtio-net
# card (Wolfser's network) and an ivshmem surface (Wolfser's window).
#
# Usage: ./build_and_qemu.sh [debug|release] [--no-run] [--viewer]
#   profile defaults to release.
#   --no-run  stop after the build.
#   --viewer  start the Qt host viewer (the default); it opens immediately and
#             waits for Wolfser, so it works on KDE, GNOME, Wayland and X11.
#   --no-viewer  keep QEMU console-only.
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
VIEWER=1
for argument in "$@"; do
    case "$argument" in
        debug | release) PROFILE="$argument" ;;
        --no-run) RUN=0 ;;
        --viewer) VIEWER=1 ;;
        --no-viewer) VIEWER=0 ;;
        *) echo "usage: $0 [debug|release] [--no-run] [--viewer|--no-viewer]" >&2; exit 1 ;;
    esac
done

cargo build "--$PROFILE" --target aarch64-unknown-none \
    --no-default-features --features no_std

if [ "$RUN" -ne 1 ]; then
    echo "built: target/aarch64-unknown-none/$PROFILE/silver_microkernel"
    exit 0
fi

SHM="${WOLFSER_SHM:-/dev/shm/werewolf-window}"
VIEWER_BIN="build/window-viewer/werewolf-window"
VIEWER_PID=""
cleanup() {
    if [ -n "$VIEWER_PID" ]; then
        kill "$VIEWER_PID" 2>/dev/null || true
        wait "$VIEWER_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT INT TERM

if [ "$VIEWER" -eq 1 ]; then
    # Always run the incremental build: a stale binary would keep an old
    # protocol (the window would wait 20 s and quit before Wolfser starts).
    if command -v cmake >/dev/null 2>&1; then
        cmake -S tools/window-viewer -B build/window-viewer >/dev/null
        cmake --build build/window-viewer -j"$(nproc 2>/dev/null || echo 2)" >/dev/null
    fi
    if [ -x "$VIEWER_BIN" ]; then
        mkdir -p build
        # The window opens at once with a waiting screen; the guest publishes
        # its first frame when the user types `wolfser`.
        "$VIEWER_BIN" --shm "$SHM" >build/wolfser-viewer.log 2>&1 &
        VIEWER_PID=$!
        echo "Wolfser window: $SHM (viewer log: build/wolfser-viewer.log)"
    else
        echo "warning: $VIEWER_BIN was not built (install Qt6 + cmake);" >&2
        echo "         start it later with tools/wolfser-host.sh" >&2
    fi
fi

rm -f "$SHM"

qemu-system-aarch64 \
    -machine virt -cpu cortex-a72 -smp 2 -m 256M \
    -kernel "target/aarch64-unknown-none/$PROFILE/silver_microkernel" \
    -drive file=disk.img,format=raw,if=none,id=hd \
    -device virtio-blk-pci,drive=hd \
    -device virtio-net-pci,disable-legacy=on \
    -object memory-backend-file,id=ivshmem0,share=on,mem-path="$SHM",size=4M \
    -device ivshmem-plain,memdev=ivshmem0 \
    -display none \
    -serial mon:stdio

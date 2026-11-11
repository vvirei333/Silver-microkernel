#!/usr/bin/env bash
# Builds the kernel together with the embedded userspace ELFs (build.rs
# re-invokes cargo inside userspace/) and boots the result in QEMU with
# disk.img attached as a virtio-blk-pci device (the kernel SFS), a virtio-net
# card (Wolfser's network) and an ivshmem surface (Wolfser's window).
#
# Usage: ./build_and_qemu.sh [debug|release] [--no-run] [--viewer|--no-viewer]
#                            [--no-linux] [--linux-tests] [--phosh]
#   profile defaults to release.
#   --no-run  stop after the build.
#   --viewer  start the Qt host viewer (the default); it opens immediately and
#             waits for Wolfser, so it works on KDE, GNOME, Wayland and X11.
#   --no-viewer  keep QEMU console-only.
#   --no-linux   build without the optional Linux ABI module (foreign ELF
#                loader, Linux syscalls, `linuxrun`). The product then has no
#                Linux code linked in; this is the removal switch.
#   --linux-tests  install the Linux ABI test programs into disk.img first
#                (runs tools/linux-test.sh; needs the AArch64 cross compiler).
#                The base image only has the OS files, so `linuxrun` targets
#                such as /sfs/linux-wayland exist only after this step.
#   --phosh      boot the Phosh image ($PHOSH_IMAGE or
#                ~/phosh-state/disk_phosh.img) instead of disk.img. At the
#                prompt run `linuxstart phosh`.
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
LINUX=1
TESTS=0
PHOSH=0
for argument in "$@"; do
    case "$argument" in
        debug | release) PROFILE="$argument" ;;
        --no-run) RUN=0 ;;
        --viewer) VIEWER=1 ;;
        --no-viewer) VIEWER=0 ;;
        --linux) LINUX=1 ;;
        --no-linux) LINUX=0 ;;
        --linux-tests) TESTS=1 ;;
        --phosh) PHOSH=1 ;;
        *) echo "usage: $0 [debug|release] [--no-run] [--viewer|--no-viewer] [--no-linux] [--linux-tests] [--phosh]" >&2; exit 1 ;;
    esac
done

FEATURES=no_std
if [ "$LINUX" -eq 1 ]; then
    FEATURES="$FEATURES,linux-abi"
elif [ "$TESTS" -eq 1 ]; then
    echo "--linux-tests needs the Linux ABI (remove --no-linux)" >&2
    exit 1
fi

cargo build "--$PROFILE" --target aarch64-unknown-none \
    --no-default-features --features "$FEATURES"

if [ "$TESTS" -eq 1 ]; then
    ./tools/linux-test.sh
fi

if [ "$RUN" -ne 1 ]; then
    echo "built: target/aarch64-unknown-none/$PROFILE/silver_microkernel"
    exit 0
fi

SHM="${WOLFSER_SHM:-/dev/shm/werewolf-window}"
IMAGE="disk.img"
if [ "$PHOSH" -eq 1 ]; then
    IMAGE="${PHOSH_IMAGE:-$HOME/phosh-state/disk_phosh.img}"
    if [ ! -f "$IMAGE" ]; then
        echo "phosh image not found: $IMAGE (set PHOSH_IMAGE)" >&2
        exit 1
    fi
fi
VIEWER_BIN="build/window-viewer/werewolf-window"
VIEWER_PID=""
cleanup() {
    if [ -n "$VIEWER_PID" ]; then
        kill "$VIEWER_PID" 2>/dev/null || true
        wait "$VIEWER_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT INT TERM

# Remove the stale surface *before* the viewer starts: otherwise a viewer
# launched against the previous run's inode would watch a file QEMU has
# already unlinked.
rm -f "$SHM"

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
        # </dev/null: the viewer must never take the terminal input that belongs to
        # QEMU's serial console.
        "$VIEWER_BIN" --watch --shm "$SHM" </dev/null >build/wolfser-viewer.log 2>&1 &
        VIEWER_PID=$!
        if [ "$PHOSH" -eq 1 ]; then
            echo "Phosh: at the prompt type \`linuxstart phosh\` (log: build/wolfser-viewer.log)"
        else
            echo "Wolfser: the window opens when you type \`wolfser\` (log: build/wolfser-viewer.log)"
        fi
    else
        echo "warning: $VIEWER_BIN was not built (install Qt6 + cmake);" >&2
        echo "         start it later with tools/wolfser-host.sh" >&2
    fi
fi

qemu-system-aarch64 \
    -machine virt -cpu cortex-a72 -smp 2 -m 1024M \
    -kernel "target/aarch64-unknown-none/$PROFILE/silver_microkernel" \
    -drive file="$IMAGE",format=raw,if=none,id=hd \
    -device virtio-blk-pci,drive=hd \
    -device virtio-net-pci,disable-legacy=on \
    -object memory-backend-file,id=ivshmem0,share=on,mem-path="$SHM",size=4M \
    -device ivshmem-plain,memdev=ivshmem0 \
    -display none \
    -serial mon:stdio

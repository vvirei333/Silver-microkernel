#!/usr/bin/env bash
# Starts the Wolfser host window: a Qt6 program that maps the ivshmem file
# QEMU exposes and forwards keys/wheel to the guest.
#
# Works on KDE and GNOME, Wayland and X11. Qt chooses the platform
# automatically; set QT_QPA_PLATFORM=wayland or xcb to force one.
#
# Usage: tools/wolfser-host.sh [--snapshot OUT.png] [--timeout SECS]
#        WOLFSER_SHM=/dev/shm/werewolf-window tools/wolfser-host.sh
set -euo pipefail

cd "$(dirname "$0")/.."

SHM="${WOLFSER_SHM:-/dev/shm/werewolf-window}"
BIN="build/window-viewer/werewolf-window"

if command -v cmake >/dev/null 2>&1; then
    # Incremental: picks up a changed viewer without a stale binary.
    cmake -S tools/window-viewer -B build/window-viewer >/dev/null
    cmake --build build/window-viewer -j"$(nproc 2>/dev/null || echo 2)" >/dev/null
elif [ ! -x "$BIN" ]; then
    echo "error: $BIN is missing and cmake is not installed" >&2
    exit 1
fi

exec "$BIN" --shm "$SHM" "$@"

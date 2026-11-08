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

if [ ! -x "$BIN" ]; then
    echo "building the host viewer (needs cmake and Qt6 Widgets)..."
    cmake -S tools/window-viewer -B build/window-viewer >/dev/null
    cmake --build build/window-viewer -j"$(nproc 2>/dev/null || echo 2)" >/dev/null
fi

exec "$BIN" --shm "$SHM" "$@"

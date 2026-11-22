# Denial WM on WerewolfOS

> Status: PARTIALLY IMPLEMENTED — the stack boots and drives the kernel's
> graphics path; the first rendered desktop frame is the current milestone.
> Denial is a standalone Wayland compositor project (denialwm/denial), not a
> theme or a shell over another compositor, and is unrelated to Phosh.

## What Denial is

deniald is a Wayland compositor (Smithay) with an **embedded Flutter engine**
for its UI. The UI is an AOT Dart bundle (`libapp.so`) driven by the
engine's threads; the compositor side is Rust. Denial ships its own Flutter
engine fork (denialwm/flutter, branch `denial/3.47.5`).

## How it talks to Silver

```
deniald (Linux task, glibc)
 ├─ Smithay DRM backend ──► kernel DRM/KMS (drm.rs, virtio-gpu)
 │    atomic commits, fencing props, syncobjs, dumb buffers
 ├─ EGL/GLES ──► Mesa llvmpipe ──► GBM ──► kernel udmabuf + DMA-BUF sync files
 ├─ Wayland frontend (wayland-1) ──► kernel AF_UNIX + SCM_RIGHTS
 ├─ libseat/seatd ──► kernel AF_UNIX listener
 ├─ Flutter engine threads ──► clone/futex/epoll/timerfd semantics
 └─ evdev/libinput ──► /dev/input (not wired yet, PLANNED)
```

Kernel UAPI implemented for this stack (all IMPLEMENTED, verified by
deniald reaching each stage):

- DRM modesetting + atomic commits (TEST_ONLY and real), plane
  IN_FENCE_FD / CRTC OUT_FENCE_PTR properties, page-flip events, syncobj
  family, DRM_CAP_SYNCOBJ.
- udmabuf controller: UDMABUF_CREATE over memfds (modern ioctl number),
  DMA_BUF_IOCTL_EXPORT_SYNC_FILE (signalled eventfd-backed fence fds) —
  the chain llvmpipe needs for EGL_ANDROID_native_fence_sync.
- glibc-level compatibility: ENOTTY/EBADF isatty semantics, STATX_MNT_ID +
  fdinfo, signalfd, logind-absent paths (a libsystemd shim is staged into
  the image; see DESIGN_DECISIONS.md).

## Boot path

`tools/build_glibc_denial.py` stages one SFS image with the Alpine boot
chain, the Debian arm64 glibc runtime, the denial release packages and the
engine; `boot-session` runs seatd, then
`deniald --flutter-bundle /usr/lib/denial/flutter --wayland --no-xwayland
--software-rendering`. Software rendering (llvmpipe, LP_NUM_THREADS=1) is
deliberate for QEMU; the same UAPI path serves real GPUs later.

## Current stop (see STATUS.md)

The engine session starts (display/raster/io threads alive, idle in futex
waits) but the first frame is not produced: no DRM atomic commit is issued
after engine start, and scanout buffers stay black. Investigation is split
between the compositor pipeline (external Wayland client probe `/sfs/wl-client`)
and Denial's KMS frame-clock mode, where the compositor answers the
engine's vsync requests itself.

## Layers Denial brings to the table

| Layer | Role |
|---|---|
| Wayland frontend | clients, surfaces, buffers via shm/dmabuf |
| Compositor core | scene graph, output topology, damage |
| Flutter runtime | engine host, platform channels, texture registry |
| Authentication | PAM/logind-absent fallbacks, lock screen |
| Portals/services | portal IPC, notification server, settings |

These run as part of deniald; WerewolfOS treats Denial exactly like any
other Linux desktop stack — no special kernel knowledge.

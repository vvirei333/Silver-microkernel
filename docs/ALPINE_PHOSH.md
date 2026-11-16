# Alpine-only image with Phosh/phoc

Status: 2026-11-17, first slice. `docs/ALPINE_OPENRC.md` merged the Alpine
rootfs with the Arch-staged Phosh closure. This slice drops the Arch tree and
packages **Alpine's own `phosh` + `phoc`** (467 packages) into an SFS image
built from scratch.

```bash
tools/build_alpine_phosh.py --install   # apk add phosh phoc alpine-base openrc dbus
tools/build_alpine_phosh.py             # repack from the existing stage
```

Stage: `/tmp/alpine-phosh`. Image: `/tmp/werewolf-alpine-phosh.img` (2 GiB,
32768 inodes). Boot: `linuxrun /sfs/sbin/init`.

## What the image contains

- Alpine 3.24.4 base, OpenRC 0.63.2, busybox, musl, `dbus`;
- `phoc` (wlroots compositor) and `phosh` (GTK4 shell) from Alpine community;
- 7917 files and 1074 symlinks; nothing skipped (see the limits below);
- `/etc/apk/repositories`, `/etc/apk/arch`, apk keys, `/etc/machine-id`.

`build_alpine_phosh.py` runs the post-install steps apk triggers would have
run: busybox applet symlinks, `/sbin/init`, and the glib schema compilation
(the latter happens in the guest because qemu-user cannot relocate Alpine's
musl `libgio`).

## Boot chain

```
/sbin/init (busybox)
  └─ /sbin/boot-session
       ├─ openrc sysinit >/tmp/openrc-sysinit.log 2>&1
       ├─ compile GSettings schemas into /tmp/gschemas
       └─ exec phoc -S -E /usr/bin/phosh
```

Environment: `WLR_BACKENDS=headless`, `WLR_RENDERER=pixman`,
`WLR_LIBINPUT_NO_DEVICES=1`, `LIBSEAT_BACKEND=builtin`,
`XDG_RUNTIME_DIR=/tmp`, `GDK_BACKEND=wayland`, `GSK_RENDERER=cairo`.
Headless + pixman is deliberate: the kernel has no DRM/KMS device for Linux
tasks yet.

## Kernel work this slice needed

| Symptom | Cause | Fix |
|---|---|---|
| `libgallium` failed to relocate | `libLLVM` (170 MiB) and `libwebkitgtk` (~70 MiB) exceeded the 8 MiB file cap | `vfs::MAX_FILE_BYTES` is 256 MiB; native whole-file exec reads stay capped at 32 MiB, the interpreter read at 8 MiB |
| 12 MiB reads were untested | double-indirect ends at ~8.06 MiB | `linux-bigfile` now uses a 12 MiB payload, exercising the triple-indirect block |
| `cp: Bad address` / `Out of memory` | `execve` capped `argv`/`envp` at 32 entries and 255-byte strings; OpenRC's environment is larger | Linux arg arrays allow 1024 entries and 4 KiB strings (256 KiB total), while the native spawn protocol keeps 32 |
| `cp` copied one chunk forever | `sendfile` used `pread` but never advanced the input offset when `offset == NULL` | `sendfile` seeks the input to the copied position; `copy_file_range` honours and preserves per-descriptor offsets |
| `cp: Bad address` (first form) | neither syscall existed | `sendfile` and `copy_file_range` implemented over kernel buffers with the same RamFs quota check |

## Evidence

Boot log of the Alpine image:

```
cp=0                       # schemas copied with busybox cp (sendfile)
files=87
compile=0
-rw-r--r-- ... /tmp/gschemas/gschemas.compiled (99637 bytes)
phoc-server-Message: Linux dmabuf support unavailable
Running compositor on wayland display 'wayland-0'
phoc-output-Message: Output 'HEADLESS-1' added
phoc-server-Message: Enabling shell mode
```

So the Alpine compositor initialises a headless output, owns the Wayland
socket and switches to shell mode (it execs `/usr/bin/phosh` from there).

## Limits / next

- No visible output: the headless backend has no scanout. The next slice is
  the same ivshmem surface the custom stack uses (or a DRM/KMS device for
  Linux tasks), so frames can be captured.
- `dconf` logs `failed to allocate file '/tmp/dconf/user': Invalid argument`
  repeatedly. A writable `MAP_SHARED` mapping of a RamFs file is likely
  refused; dconf falls back to memory but the shell may store no settings.
- `phosh` starting after "Enabling shell mode" is not yet verified: the log
  ends there. Needs the session bus/logind question resolved (Alpine's phosh
  expects elogind; the image has none).
- `libLLVM` is only needed by Mesa's `libgallium`; a pixman-only image could
  drop it later and shrink the image by ~170 MiB.
- The two `>64 MiB` libraries make packing slower and need the 256 MiB cap;
  a package-level trim is preferable in the long run.

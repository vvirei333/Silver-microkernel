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
socket, switches to shell mode and serves `wayland-0`. dconf stays quiet
after the positional-write fix.

## Scanout bridge (work in progress)

`tools/screencopy.c` is a wlr-screencopy client: it connects to phoc's
`wayland-0`, captures the headless output in shm, converts ARGB8888 to RGBA
and publishes frames in the Silver window layout (magic `WIN1`, header at
0x1000, pixels at 0x1000). It is built for glibc and deployed with its own
runtime under `/opt/glibc` (`ld-linux`, `libc`, `libwayland-client`,
`libffi`), because Alpine's musl headers are not installed in the stage.

The tool is ready, but it cannot be exercised yet: on this ABI **phoc itself
crashes** about a second after `HEADLESS-1` is added, with or without
`-S -E phosh`, with 1 GiB and 2 GiB of guest RAM, debug logging on or off:

```
phoc-output: Output 'HEADLESS-1' added ...
Task 8 killed: Page Fault at 0x00000000000000E9 (ESR: 0x92000006)
ELR_EL1=0000000010046AF0          (phoc text)
```

The fault is the same instruction every time (`ELR 0x46AF0`, phoc text). With
the matching `phoc-dbg` package that resolves to
`phoc_xdg_toplevel_constructed` (`src/xdg-toplevel.c`), at the register of
`wl_signal_add (&self->xdg_toplevel->base->surface->events.commit, ...)`:
`self->xdg_toplevel` is a small garbage value (`0x11`) that passes the
`g_assert (self->xdg_toplevel)` because it is non-NULL.

It reproduces with **no client at all** (`phoc -E /bin/true`, not even a
shell), with `-S -E phosh`, at 1 GiB and 2 GiB of RAM, and with Xwayland
explicitly disabled (`--no-xwayland`). A standalone memfd test (four 3 MiB
`memfd_create` + `ftruncate` + `mmap` + memset runs) passes at the same point
in boot, so a simple contiguous-allocation failure is not the whole story.

The kernel now dumps the dying task's register bank on a user fault. At the
crash:

```
x0=0x0000000000000011  x19=0x0000000044A41EA0  x29=0x000000007FE3F640
x30=0x0000000020051C44  SP_EL0=0x000000007FE3E400
```

`x0` is the garbage pointer being followed; `x19` is the `PhocXdgToplevel`
object and `x30` returns into a library around 0x2005_xxxx (GObject/GIO
range). The crash is timing-sensitive: with `WLR_DEBUG=1` and
`G_MESSAGES_DEBUG=all` phoc survived past output creation in one run, and
then served clients. That combination (garbage object property, timing
dependence, no syscall error) points at memory corruption inside phoc rather
than a clean error return.

When phoc dies, its Wayland listener disappears with the task: clients then
get `ECONNREFUSED` on `/tmp/wayland-0` even though the socket node is still
visible, which is exactly what the screencopy bridge reported. Next steps: a
gdb remote stub (the register dump is already in place) to catch the write
that installs `0x11`, and an audit of the syscalls phoc uses between output
creation and the first toplevel construction.

## apk inside the guest

The Alpine-only image carries a real package database and keys, and `apk`
runs under the Linux ABI:

```
howl> linuxrun /sfs/apk-info          # static launcher for apk --no-network info
abseil-cpp-base
...
```

`tools/alpine_apk_smoke.py` injects the launcher, boots the image and asserts
that `alpine-baselayout` appears in the listing (`PASS apk info`). The
openrc image (`ALPINE_OPENRC.md`) cannot do this yet: it merges the Arch
closure, whose glibc `libz.so.1`/`libssl.so.3` shadow Alpine's musl builds,
so apk fails to relocate. The Alpine-only image is the right home for apk.

## Limits / next

- No visible output yet: the headless backend has no scanout, and the
  screencopy bridge is blocked by the phoc crash described above. Once phoc
  stays up, the same ivshmem surface the custom stack used will show frames.
- dconf is fixed: `pwrite(fd, "", 1, 1)` into a fresh file returned `EINVAL`
  because both the Linux `pwrite` path and the RamFs `write_at` refused
  offsets past EOF. Positional writes now zero-fill the hole and do not move
  the descriptor offset, matching Linux; the `dconf-CRITICAL` messages are
  gone.
- The compositor serves clients: `/tmp/wayland-0` exists 45 s into the boot
  and `phoc` logs "Enabling shell mode". Whether the `phosh` process stays
  alive after that is still unverified (the log shows several tasks exiting
  with status 0 around that point).
- `phosh` staying alive after "Enabling shell mode" is not yet verified. It
  needs the session bus/logind question resolved (Alpine's phosh expects
  elogind; the image has none) and a way to observe the process tree.
- `libLLVM` is only needed by Mesa's `libgallium`; a pixman-only image could
  drop it later and shrink the image by ~170 MiB.
- The two `>64 MiB` libraries make packing slower and need the 256 MiB cap;
  a package-level trim is preferable in the long run.

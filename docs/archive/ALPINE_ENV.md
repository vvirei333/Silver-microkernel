# Alpine userland on the Linux ABI (first slice)

The Linux ABI layer is deliberately small; Alpine is the base the
postmarketOS/Nura userspace is built from. This slice proves the smallest
useful Alpine environment runs as Linux EL0 tasks from SFS, with **musl** and
without the Arch/glibc staging.

## What runs

| File in `/sfs` | Proof |
|---|---|
| `busybox` | Alpine 3.24 BusyBox v1.37, dynamically linked against musl; prints its usage |
| `sh` | same binary copied under the shell applet name; starts an ash prompt |
| `apk` | `apk-tools 3.0.9-r0, compiled for aarch64` (static PIE) |
| `ld-musl-aarch64.so.1` | musl loader/interpreter |
| `libc.musl-aarch64.so.1` | same bytes, because SFS has no symlinks and BusyBox has `NEEDED libc.musl-aarch64.so.1` |
| `etc/alpine-release`, `etc/os-release` | Alpine identity |

Build and smoke test:

```bash
tools/build_alpine_env.py          # disk.img -> /tmp/werewolf-alpine.img
tools/alpine_smoke.py              # PASS alpine busybox / PASS alpine apk-tools
```

The minirootfs is cached in `/tmp/alpine-env`; the tracked `disk.img` is never
modified. The builder pins Alpine 3.24.4 and apk-tools 3.0.9-r0, extracts only
the files above, patches BusyBox's interpreter to
`/sfs/ld-musl-aarch64.so.1` and rpath to `/sfs`, and injects into an SFS copy.

## Why this shape

- SFS names were limited to 27 bytes when this slice ran (long names up to
  255 and symlinks landed later, see `docs/ALPINE_OPENRC.md`); the builder
  installs the musl loader twice instead of relying on a `libc.musl` symlink.
- Linux ABI already supports ET_DYN/PT_INTERP and the `/sfs` read prefix;
  musl and glibc binaries coexist as long as each ELF names its own loader.
- Alpine's busybox is ~1 MiB and musl ~0.7 MiB, so the base stays small; the
  large package closure only appears when a UI (Phosh) is installed.

## Limits / next

- `apk` runs, but actual `apk add` needs `/etc/apk/repositories`, Alpine keys,
  DNS/HTTP from the guest and enough filesystem semantics; that is the next
  slice.
- The full Alpine rootfs (OpenRC/systemd, /usr, PAM, udev) is not installed;
  this is the package-manager + shell base, not a bootable pmOS rootfs.
- Phosh/phoc on Alpine is the next large closure. The Arch-staged Phosh image
  remains the working desktop until the Alpine closure is complete.
- The kernel still owns SFS/virtio-blk; Alpine sees only the Linux ABI.

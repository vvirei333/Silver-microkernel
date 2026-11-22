# Filesystems

> Status markers follow [STATUS.md](STATUS.md).

## SFS — Silver File System — IMPLEMENTED

SFS is the native, deliberately simple filesystem of the kernel.

- **Format** (`sfs.rs`, `sfs_format.rs`): superblock, inode/bitmap/data
  regions, fixed 512-byte block addressing, single/double/triple indirect
  blocks, symlinks and hardlink-style duplicates supported by the injector.
- **Kernel side**: read-only image mount at boot (QEMU disk or memory), a
  bounded read cache (`cache.rs`, `cache_policy.rs`), file creates/writes
  allowed only in designated scratch subtrees.
- **Host side** (`scripts/`): `make_sfs.py` creates images;
  `sfs_inject.py` writes/unpacks trees into images (used by every image
  builder); `sfs_fsck.py` validates structure; `sfs_resize.py` grows
  images.
- **Verification**: kernel VFS self-test at boot; fsck on every image the
  builders produce.

Properties and limits: no journaling, no crash recovery guarantees, no
permissions model beyond the kernel's mount policy, files bounded by the
indirect-block tree (multi-GiB images verified). crash consistency is
PLANNED (not designed yet).

## RamFs / scratch tree — PARTIALLY IMPLEMENTED

Linux tasks expect a writable `/tmp`. The kernel serves writes to the
scratch subtree through in-kernel buffers instead of a separate in-memory
filesystem type. There is no standalone RamFs implementation; if a real
RamFs is needed (e.g. for `/run`), it is PLANNED.

## Synthetic trees — IMPLEMENTED

These are kernel-served, not stored on disk:

| Tree | Contents |
|---|---|
| `/proc` | per-task fdinfo, self/maps, self/environ, cpuinfo, sys/kernel/osrelease (subset used by glibc/GLib) |
| `/sys` | DRM device descriptions (vendor, render/card nodes) — subset needed by libdrm/Mesa |
| `/dev` | console, null, zero, full, urandom/random, tty*, dri/card0 + renderD128 (+ by-path aliases), udmabuf, ivshmem, gpufb, input (empty today) |
| `/dev/shm` | POSIX shm named objects backed by shared memfds |

## Linux view

Linux tasks see the SFS root as `/` (the kernel maps user paths onto the
mount). Alpine (boot chain) and Debian trixie (glibc runtime) trees are
staged into one SFS image by `tools/build_glibc_denial.py`; the layout is a
normal Linux rootfs with `/lib/ld-linux-aarch64.so.1` wired to the Debian
loader.

## virtio-blk — IMPLEMENTED (device side)

The kernel probes virtio-blk via FDT and registers the disk image as the SFS
backing store. No partitioning, no write-back to the host image (the guest
image is immutable; writes go to scratch). A persistent writable SFS is
PLANNED.

# Alpine + OpenRC + Phosh on the Linux ABI

Status: M3 userspace slice. `docs/ALPINE_ENV.md` covered the smallest useful
Alpine (BusyBox + musl + static apk). This slice merges a full Alpine rootfs,
OpenRC 0.63.2 and the existing Phosh closure into one SFS image and boots it as
a Linux EL0 world: `/sbin/init` is BusyBox init, `/etc/inittab` starts OpenRC,
and OpenRC's `phosh` service launches the desktop.

Builder:

```bash
tools/build_alpine_openrc.py            # /home/ivan/phosh-state/disk_phosh.img
tools/build_alpine_openrc.py --force    # rebuild the Alpine stage from scratch
```

Output is `/tmp/werewolf-alpine-openrc.img` (3 GiB). The tracked `disk.img`
and the 2 GiB Phosh source image are never modified. Boot:

```bash
tools/linuxrun.py /sfs/sbin/init        # ivshmem transport, frame dumps to /tmp
```

## Root layout: one filesystem, two names

Alpine's FHS paths and the Silver SFS paths must both work, and the Arch-staged
Phosh closure already lives under `/sfs`. So Linux `/` **is** SFS `/sfs`:

| Linux path | Actually resolves to | Why |
|---|---|---|
| `/etc`, `/bin`, `/sbin`, `/usr`, `/lib` | `/sfs/etc`, ... | Alpine rootfs image |
| `/sfs/...` | unchanged | legacy Phosh closure, `phosh-serve` |
| `/tmp`, `/run`, `/var` | RamFs | writable scratch, no SFS writes |
| `/dev`, `/proc`, `/sys` | synthetic | device nodes, `procfs.rs` |

The global aliases (`/sfs`, `/tmp`, `/dev`, `/proc`, `/sys`) stay visible so
paths compiled into the Phosh binaries keep resolving.

## SFS: symlinks

Alpine is full of symlinks (`/bin/sh -> busybox`, applet links, runlevel
links, `libc.musl-aarch64.so.1`). SFS now has a symlink inode:

- inode kind `3`, target bytes stored in the normal data blocks;
- `resolve` follows links with a hop budget so cycles cannot spin;
- `read_link`, `lstat_kind` and `link_len` added to the VFS trait;
- `scripts/sfs_inject.py` grew `write_link_at()`, and freeing a symlink frees
  its data blocks like any other inode.

SFS name limit stays 27 bytes. Long Alpine paths (notably `apk` signing keys
with long hex names) are skipped at pack time and reported by the builder;
nothing that boots needs them today.

`scripts/sfs_fsck.py` understands kind 3 and passes on the built image.

## Kernel fixes this slice needed

Every one of these was found by running the real thing, not by inspection.

| Symptom | Fix |
|---|---|
| `execve` of a `#!/bin/sh` script fell over | shebang parsing in `linux_execve` re-execs the interpreter from the script's `argv` |
| BusyBox/musl read empty `/proc` | synthetic `src/procfs.rs`: `cmdline`, `filesystems`, `mounts`, `mountinfo`, `self/*`, `sys/kernel/hotplug` |
| mounts failed | `mount`/`umount2`: tmpfs maps to RamFs, proc/sysfs/devtmpfs are accepted no-ops |
| `console` open failed | `/dev/console` target in `linux_fd` |
| OpenRC saw the wrong PID | PID-1 view: `/sbin/init` sees itself as 1, children numbered from there |
| musl `vfork` used a stack we ignored | vfork emulation honours the stack musl's `__clone` passes |
| a signals-heavy binary lost `x0` after a handler | `rt_sigreturn` no longer overwrites `frame[0]` (`syscall.rs` return path) |
| OpenRC's `deptree` was always 0 bytes | `writev` accepted only the console; musl stdio writes *every* stream through `writev` — it now covers any descriptor. `readv` (musl's stdio read path) was missing entirely and is implemented too |
| `fdopen()` over a file failed with `EINVAL` | `fcntl(F_GETFL)` reported a hardcoded `O_RDONLY`; the file's `O_ACCMODE` is now recorded on `open` and survives `dup`/`dup2` |
| `stat` on SFS files reported non-executable | SFS reports `0555` (SFS stores no modes); RamFs paths report `0644` |

## OpenRC

`/etc/inittab`:

```
::sysinit:/sbin/boot-session
```

`/sbin/boot-session` (written by the builder):

```sh
#!/bin/sh
/sbin/openrc sysinit >/tmp/openrc-sysinit.log 2>&1
exec /sbin/rc-service phosh start 2>/tmp/phosh-start.log
```

The two redirections are not cosmetic: on this ABI an OpenRC process that
writes its progress to the console does not return, so sysinit would block
busybox init forever. With output to a file `openrc sysinit` exits with 0 and
the session service becomes the console owner. `exec` keeps the pid that init
waits on.

`openrc sysinit` runs cleanly and the dependency cache works: `rc`
regenerates `deptree` (1508+ bytes) and `rc-update`/`rc-service` see the real
dependency graph. The `default` runlevel is still **not** used: `openrc boot`
and `openrc default` block before printing anything on this ABI, so the
session service is started by name (`rc-service phosh start`). `rc-update
show` lists `phosh | default`. Fixing the runlevel transition (and the
console-write stall behind it) is the next OpenRC slice.

Service definition, `/etc/init.d/phosh`:

```sh
#!/sbin/openrc-run
description="Phosh on the Silver Wayland surface"
command="/sfs/phosh-serve"
command_background=true
pidfile="/run/phosh.pid"
```

`XDG_RUNTIME_DIR` stays `/tmp` (the writable scratch tree the Wayland
stack binds its sockets in); the service does not override it.

## Evidence

Boot of `linuxrun /sfs/sbin/init` with the ivshmem window: the guest boots
busybox init, runs OpenRC `sysinit`, then `rc-service phosh start`; Phosh
publishes its lock screen to the shared window. Captured frame:
[`assets/alpine-openrc-lock.png`](../assets/alpine-openrc-lock.png). The full
chain takes about two to three minutes, mostly `rc` regenerating `deptree`
because every file reports mtime 0 (clock skew).

Kernel-side gates after these changes:

- `cargo test --lib` 226/226;
- `python3 tools/security_test.py` 12/12;
- `python3 tools/linux_matrix.py` (see `handoff.md` for the current count);
- `./build_and_qemu.sh release --no-linux --no-run` clean — no Linux code in
  the bare kernel.

## Known limits

- `openrc boot`/`openrc default` block silently, and any OpenRC process that
  writes progress to the console does not return; `boot-session` redirects
  sysinit's output and starts the service by name.
- Boot regenerates `deptree` once per `rc` invocation (mtime 0 everywhere),
  which dominates the boot time.
- The clock is at 0, so `rc` regenerates `deptree` on every invocation and
  prints a clock-skew warning; harmless, but it slows boot.
- apk repositories/keys are not installed; `apk` runs but cannot fetch.
- SFS has no modes, no ownership, no timestamps beyond what the inode carries.
- `--no-linux` builds carry no rootfs: Alpine only exists behind the Linux ABI.
- Phosh here is the Arch-staged closure from `disk_phosh.img`; replacing it
  with Alpine's `phosh`/`phoc` packages is the next userspace slice.

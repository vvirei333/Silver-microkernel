# Contributing to WerewolfOS

WerewolfOS is a console-first microkernel: the kernel, drivers, filesystem,
network stack and Howl stay native, while Linux applications (the Denial
desktop) run through the optional `linux-abi` module. The project direction
and milestone plan are in [`docs/VECTOR.md`](docs/VECTOR.md) and
[`docs/ROADMAP.md`](docs/ROADMAP.md).

## Ground rules

- **The Linux boundary is a build boundary.** `src/linux_*` may only be
  reachable behind `#[cfg(feature = "linux-abi")]`. `./build_and_qemu.sh
  --no-linux --no-run` must stay clean — it is the removal switch and a
  regression test for accidental Linux code leaking into the kernel image.
- **No `std`.** The kernel and userspace are `no_std`; allocations in the
  kernel go through the bounded heap, userspace uses static storage or the
  Linux ABI's `brk`/`mmap` for foreign programs.
- **A new syscall or driver needs a test.** Host unit tests live next to the
  module (`cargo test --lib`); guest proofs are boot tests, entries in the
  Linux ABI matrix (`docs/STATUS.md`) or a case under `tests/security/`.
  A parser that consumes untrusted bytes gets a host fuzz target
  (`tests/security/fuzz/`). A security fix lands with its
  failing-then-passing test — see `docs/SECURITY.md`.
- **Ports are platform trees.** One device is one file plus its driver tasks;
  the checklist and the agent workflow for a new board are in the master plan
  (M5).

## Build and run

```bash
rustup target add aarch64-unknown-none

./build_and_qemu.sh                 # release kernel + userspace, boot QEMU
./build_and_qemu.sh --no-viewer     # console only
./build_and_qemu.sh --linux-tests   # install the Linux ABI test programs
./build_and_qemu.sh --no-linux      # build without the Linux module
./build_and_qemu.sh --gpu           # add virtio-gpu for the Denial track
```

The host window viewer is built by the script (Qt6 + cmake); without it the
guest still runs and publishes frames into the ivshmem surface.

## Tests

```bash
cargo test --lib                    # kernel/host unit tests (272 today)
./build_and_qemu.sh --linux-tests   # then run the matrix at `howl>`
scripts/release_smoke.py            # packaged release gate (8 checks)
tools/security_test.py --fuzz       # boundary regressions + HTTP fuzzer
```

The Linux ABI matrix and the expected strings are listed in
[`docs/STATUS.md`](docs/STATUS.md). A change is ready when the matrix is green
and `health` reports 0 zombies, 0 open files and 0 DMA leaks. The Mobile
image is built by `tools/build_glibc_denial.py`; it boots into deniald rather
than the `howl>` prompt.

## Style

- `cargo fmt` and `cargo clippy` for Rust; keep the kernel `no_std` and
  allocation-free where the surrounding code is.
- C helpers under `tests/linux/` are freestanding (`-nostdlib`); do not add
  libc calls to code that is linked into the guest.
- Commits: one logical change, imperative subject, body explains *why* (and
  what was ruled out). Reference the milestone (M0.2, M2, …) when relevant.

## Sign-off (DCO)

Every commit must be signed off:

```bash
git commit -s
```

The `Signed-off-by: Name <email>` trailer certifies the Developer Certificate
of Origin 1.1: you wrote the change, or you have the right to submit it, and
you license it to the project under its MIT license. Use your real name and a
reachable email. Commits without a sign-off are asked to amend, not silently
merged.

## Reporting

- Bugs and regressions: open an issue with the boot log (`serial.log`), the
  exact QEMU command and the commit hash.
- Security: do not open a public issue for a memory-safety or capability
  bypass; contact the maintainer privately first. A
  `SECURITY.md` with the disclosure process lands with M2.

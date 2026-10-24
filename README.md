# silver_microkernel

A hobby microkernel for AArch64 (ARM64) written in Rust. The foundation is
bare-metal Rust (`no_std`), with drivers and services in C/C++ planned on top
of it. Cross-platform development: Windows and Linux (Arch) share the same
code — only the build commands and toolchain setup differ.

Target platform: QEMU `virt` machine (ARM Virt Board), Cortex-A72 CPU.

## What already works

- [x] Bare-metal boot: `_start`, custom linker script, `.bss` zeroing
- [x] PL011 UART console output
- [x] ARM64 exception handler: kernel panic with error code and address
- [x] Physical memory map + page frame allocator (bitmap)
- [x] Kernel heap + Rust global allocator (`alloc`: `Box`, `Vec`)
- [x] GICv2 + ARM Generic Timer (IRQ 30, 100 Hz tick)
- [x] Round-robin scheduler: dynamic `spawn`, `yield`, `sleep`, `exit`
- [x] Blocking mutex with FIFO wait queue (RAII guard)
- [x] IPC: mailbox `send()` / blocking `receive()` between tasks
- [x] System calls via `svc #0` (number in x8, arguments in x0-x5)
- [x] Nameserver: tasks find each other by name (register/lookup syscalls)
- [x] Condition variable: per-mailbox wait queues, targeted wakeups
- [x] Counting semaphore + bounded channel with backpressure
- [x] MMU: identity mapping, 4-level tables, SCTLR_EL1.M on (kernel at EL1)
- [x] Per-task address spaces: private VA window per task, TTBR0 switch on context switch
- [x] Tasks in EL0 (Ring 3): user stacks, SVC-only kernel access, argument validation
- [x] EL0 fault containment: bad user accesses kill the task, not the kernel
- [ ] Full kernel-data isolation (user tasks can still access shared kernel data)
- [ ] Drivers, processes, ELF loader

## Requirements

- Rust stable with the `aarch64-unknown-none` target
- QEMU with AArch64 system emulation

### Windows

```bash
# Rust + target
rustup target add aarch64-unknown-none

# QEMU: installer from https://www.qemu.org/download/#windows
# (path below is the installer's default)
```

### Arch Linux

```bash
sudo pacman -S qemu-system-aarch64 rustup
rustup target add aarch64-unknown-none
```

## Build and run

Windows (Git Bash):

```bash
export PATH="$(cygpath -u "$USERPROFILE")/.cargo/bin:$PATH"
cd "F:/My Projects/Silver microkernel"
cargo build --target aarch64-unknown-none --no-default-features --features no_std
timeout 5s "/c/Program Files/qemu/qemu-system-aarch64.exe" \
  -machine virt -cpu cortex-a72 -m 128M -nographic \
  -kernel target/aarch64-unknown-none/debug/silver_microkernel
```

Linux:

```bash
cargo build --target aarch64-unknown-none --no-default-features --features no_std
timeout 5s qemu-system-aarch64 \
  -machine virt -cpu cortex-a72 -m 128M -nographic \
  -kernel target/aarch64-unknown-none/debug/silver_microkernel
```

The kernel never exits on its own: `timeout` kills it, so exit code `124`
means "the kernel kept running the whole time" — that is success.

`Cargo.toml` has two features: `std` (default, a stub to keep the host
toolchain happy) and `no_std` (the real kernel build). The kernel only builds
with `--no-default-features --features no_std`.

## Project layout

| File | Purpose |
| --- | --- |
| `src/main.rs` | `_start` entry point, IRQ trampoline, subsystem init |
| `src/exceptions.rs` | ARM64 exception vectors, kernel panic |
| `src/pl011.rs` | UART driver (MMIO 0x09000000) |
| `src/memory.rs` | Physical memory map |
| `src/frame_allocator.rs` | Physical page frame allocator (bitmap) |
| `src/allocator.rs` | Kernel heap, `#[global_allocator]` |
| `src/gic.rs` | GICv2 interrupt controller |
| `src/timer.rs` | ARM Generic Timer, tick counter |
| `src/interrupts.rs` | IRQ control: masking/restoring DAIF |
| `src/scheduler.rs` | Scheduler: tasks, context switch, demo tasks |
| `src/mutex.rs` | Blocking mutex with wait queue |
| `src/ipc.rs` | Mailbox IPC: `send` / `receive` |
| `src/syscall.rs` | System call table and `svc #0` wrappers |
| `src/nameserver.rs` | Nameserver task: name → task ID registry over IPC |
| `src/condvar.rs` | Condition variable: wait / notify_one / notify_all |
| `src/semaphore.rs` | Counting semaphore on mutex + condvar |
| `src/channel.rs` | Bounded channel: two semaphores + a mutex (backpressure) |
| `src/mmu.rs` | MMU: identity map, MAIR/TCR/TTBR, SCTLR_EL1.M, per-task address spaces |
| `.cargo/config.toml` | Target, rust-lld linker, `-Tlinker.ld` flag |

Per-task spaces: every task gets its own L0 table chained to a shared
identity-mapped kernel half plus a private 4 KB-mapped window at
`0x0000_0080_0000_0000`. The scheduler switches TTBR0_EL1 (with
`tlbi vmalle1`) on every context switch that crosses address spaces.
The `vm_a` / `vm_b` demo tasks prove isolation: both write to the same
virtual address and read back only their own values.

All demo tasks except idle run at EL0t on separate user stacks. They reach
the kernel only through `svc #0`; UART, GIC and other MMIO are inaccessible
at EL0. The syscall layer validates task pointers before dereferencing them.
An EL0 exception that is not an SVC prints a task-fault report and terminates
only the offending task.

Architecture details, invariants and the roadmap: [handoff.md](handoff.md).

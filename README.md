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
- [x] MMU: 4-level tables, 4 KB granules, high-half kernel via TTBR1
- [x] Per-task TTBR0 spaces: private page, fixed-VA user stack, TTBR0 switch on context switch
- [x] Kernel data, heap, kernel stacks and MMIO are absent from user TTBR0
- [x] Tasks in EL0 (Ring 3): user stacks, SVC-only kernel access, argument validation
- [x] EL0 fault containment: bad user accesses kill the task, not the kernel
- [x] High-half isolation probe: an EL0 read faults without stopping other tasks
- [ ] Separate user ELF binaries (removes the temporary read-only text/rodata alias)
- [ ] Drivers and full process lifecycle

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
| `src/main.rs` | Physical boot entry, high-half transition, exception trampolines |
| `src/exceptions.rs` | ARM64 exception vectors, kernel panic |
| `src/pl011.rs` | UART driver (physical MMIO 0x09000000) |
| `src/memory.rs` | Physical memory map and high-half address conversion |
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
| `src/mmu.rs` | TTBR1 kernel map, per-task TTBR0 maps, user pointer validation |
| `.cargo/config.toml` | Target, rust-lld linker, `-Tlinker.ld` flag |

## Memory model

The kernel is linked at high virtual addresses but loaded at low physical
addresses. The assembly boot stub builds temporary low/high aliases, enables
the MMU, branches to the high PC, and removes the kernel's low identity map.

```text
TTBR1 (EL1 only):  0xFFFF000000000000 + PA -> kernel image, heap, stacks, MMIO
TTBR0 (per task): read-only image alias + own fixed-VA stack + private page
```

The scheduler changes only TTBR0_EL1 and flushes stale TLB entries. Its EL1
code and SP_EL1 stack remain mapped through TTBR1_EL1. Every user task gets
an 8 KB stack at virtual address `0x7FE00000` and a private 4 KB page at
`0x0000_0080_0000_0000`; equal VAs in different tasks have different physical
backing. The `vm_a` / `vm_b` demo proves this by reading back only its own
value.

The built-in `.text` and `.rodata` still have a **read-only low alias** in
TTBR0 because the demo tasks are statically linked with the kernel. Writable
kernel data, heap, kernel and user-task kernel stacks, page tables and MMIO
are not mapped there. A separate user ELF loader will remove this final
shared alias.

All demo tasks except idle run at EL0t and enter the kernel only through
`svc #0`. Syscall pointers must lie in the shared image, the current task's
stack, or its private page. The intentional isolation probe reads
`0xFFFF000040000000` from EL0: the expected result is one `TASK FAULT` with
`EC=0x24`, after which all other tasks continue running.

Architecture details, invariants and the roadmap: [handoff.md](handoff.md).

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
- [x] Round-robin scheduler: dynamic `spawn(program, argument)`, `yield`, `sleep`, `exit`
- [x] Lifecycle cleanup: address spaces, mailboxes, wait queues and owned mutexes
- [x] Blocking mutex with FIFO wait queue, ownership tracking and RAII guard
- [x] IPC: mailbox `send()` / blocking `receive()`, selective nameserver RPC receive
- [x] System calls via `svc #0` (number in x8, arguments in x0-x5)
- [x] Nameserver: register/lookup/unregister plus stale-owner cleanup
- [x] Condition variable: per-mailbox wait queues, targeted wakeups
- [x] Counting semaphore + bounded channel with backpressure
- [x] MMU: 4-level tables, 4 KB granules, high-half kernel via TTBR1
- [x] Two userspace ELFs with checked `PT_LOAD` loading and BSS zeroing
- [x] Per-task TTBR0 spaces: independent ELF image, fixed-VA stack and private page
- [x] W^X user mappings: R-X text, R-- rodata, RW-NX data/stack/private memory
- [x] No kernel image, kernel data, heap, kernel stacks or MMIO in user TTBR0
- [x] Tasks in EL0 (Ring 3): user stacks and SVC-only kernel access
- [x] Checked copyin/copyout with permission validation, `LDTR/STTR` and PAN enabled
- [x] EL0 fault containment: bad user accesses kill the task, not the kernel
- [x] High-half and removed-alias probes: both fault without stopping other tasks
- [ ] Drivers and richer process management (kill, wait, credentials)

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
with `--no-default-features --features no_std`. For a bare-metal build,
`build.rs` automatically builds `userspace/` for AArch64 and embeds both the
demo and nameserver ELFs; no separate userspace command is needed.

## Project layout

| File | Purpose |
| --- | --- |
| `build.rs` | Builds and embeds both userspace ELFs for bare-metal profiles |
| `src/main.rs` | Physical boot entry, high-half transition, exception trampolines |
| `src/exceptions.rs` | Task fault diagnostics and kernel panic |
| `src/pl011.rs` | UART driver (physical MMIO 0x09000000) |
| `src/memory.rs` | Physical memory map and high-half address conversion |
| `src/frame_allocator.rs` | Physical page frame allocator (bitmap) |
| `src/allocator.rs` | Kernel heap, `#[global_allocator]` |
| `src/gic.rs` | GICv2 interrupt controller |
| `src/timer.rs` | ARM Generic Timer, tick counter |
| `src/interrupts.rs` | IRQ control: masking/restoring DAIF |
| `src/scheduler.rs` | Tasks, program/argument spawn, context switch and resource reaping |
| `src/mutex.rs` | Blocking mutex with FIFO waiters and ownership tracking |
| `src/wait_queue.rs` | Shared FIFO queue with dead-waiter removal |
| `src/ipc.rs` | Mailbox IPC, selective receive and mailbox cleanup |
| `src/syscall.rs` | System call table and checked user-buffer operations |
| `src/user_memory.rs` | PAN-compatible `LDTR/STTR` copyin/copyout |
| `src/condvar.rs` | Condition variable: wait / notify_one / notify_all |
| `src/semaphore.rs` | Counting semaphore on mutex + condvar |
| `src/channel.rs` | Bounded channel: two semaphores + a mutex (backpressure) |
| `src/mmu.rs` | TTBR1 kernel map, per-task TTBR0 maps, permission validation |
| `src/elf_loader.rs` | Static ELF64/AArch64 `PT_LOAD` loader and image ownership |
| `src/user_program.rs` | Program IDs and embedded userspace ELF selection |
| `userspace/` | Separate demo and nameserver EL0 programs plus syscall wrappers |
| `.cargo/config.toml` | Target, rust-lld linker, `-Tlinker.ld` flag |

## Memory model

The kernel is linked at high virtual addresses but loaded at low physical
addresses. The assembly boot stub builds temporary low/high aliases, enables
the MMU, branches to the high PC, and removes the kernel's low identity map.

```text
TTBR1 (EL1 only):  0xFFFF000000000000 + PA -> kernel image, heap, stacks, MMIO
TTBR0 (per task): own ELF image + own fixed-VA stack + own private page
```

The scheduler changes only TTBR0_EL1 and flushes stale TLB entries. Its EL1
code and SP_EL1 stack remain mapped through TTBR1_EL1. Every user task gets
an independently loaded ELF image at `0x40000000`, an 8 KB stack at
`0x7FE00000`, and a private 4 KB page at `0x0000_0080_0000_0000`. Equal VAs
in different tasks have different physical backing. VM A/B verify both the
private page and an ELF BSS variable by reading back only their own values.

The userspace linker emits separate R-X, R-- and RW `PT_LOAD` segments. The
loader accepts static ELF64/AArch64 `ET_EXEC` files inside a 2 MB image window,
rejects W+X or incompatible page permissions, copies file bytes into fresh
frames and zero-fills BSS. There is no shared kernel `.text` / `.rodata` alias
left in TTBR0.

All demo tasks except idle run at EL0t and enter the kernel only through
`svc #0`. Copyin/copyout validates mapping and read/write permissions through
the current TTBR0, then uses unprivileged `LDTRB/STTRB` while SCTLR.PAN is
enabled. Read-only and unmapped output buffers are rejected before a receive
can block.

Two intentional probes prove isolation: task 15 reads
`0xFFFF000040000000` (high-half kernel) and task 16 reads
`0x0000000040080000` (the old kernel text alias). Each must produce one
`TASK FAULT` with `EC=0x24`; all other tasks continue running.

`spawn(program, argument)` selects a registered ELF and passes an argument to
its entry point. The lifecycle demo dynamically spawns a child, checks the
argument, registers/unregisters a name, exits while holding a mutex and proves
that reaping releases it. Nameserver RPC uses selective receive so unrelated
messages cannot be consumed as lookup responses.

Architecture details, invariants and the roadmap: [handoff.md](handoff.md).

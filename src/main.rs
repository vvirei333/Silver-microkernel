#![cfg_attr(not(feature = "std"), no_std)]
#![cfg_attr(not(feature = "std"), no_main)]

#[cfg(not(feature = "std"))]
extern crate alloc;

#[cfg(not(feature = "std"))]
mod allocator;
#[cfg(not(feature = "std"))]
mod capability;
#[cfg(not(feature = "std"))]
mod channel;
#[cfg(not(feature = "std"))]
mod condvar;
#[cfg(not(feature = "std"))]
mod elf_loader;
#[cfg(not(feature = "std"))]
mod exceptions;
#[cfg(not(feature = "std"))]
mod frame_allocator;
#[cfg(not(feature = "std"))]
mod gic;
#[cfg(not(feature = "std"))]
mod interrupts;
#[cfg(not(feature = "std"))]
mod ipc;
#[cfg(not(feature = "std"))]
mod memory;
#[cfg(not(feature = "std"))]
mod mmu;
#[cfg(not(feature = "std"))]
mod mutex;
#[cfg(not(feature = "std"))]
mod pl011;
#[cfg(not(feature = "std"))]
mod scheduler;
#[cfg(not(feature = "std"))]
mod semaphore;
#[cfg(not(feature = "std"))]
mod syscall;
#[cfg(not(feature = "std"))]
mod timer;
#[cfg(not(feature = "std"))]
mod user_memory;
#[cfg(not(feature = "std"))]
mod user_program;
#[cfg(not(feature = "std"))]
mod vfs;
#[cfg(not(feature = "std"))]
mod wait_queue;

#[cfg(not(feature = "std"))]
use core::panic::PanicInfo;

#[cfg(all(feature = "std", feature = "no_std"))]
compile_error!("Features 'std' and 'no_std' are mutually exclusive");

#[cfg(all(not(feature = "std"), not(target_arch = "aarch64")))]
compile_error!("The bare-metal kernel currently supports only AArch64");

#[cfg(all(not(feature = "std"), target_arch = "aarch64"))]
core::arch::global_asm!(
    r#"
    .section .text.boot, "ax"
    .global _start
    .type _start, %function
_start:
    // The ELF is linked at high VAs but loaded at low physical addresses.
    // Until the MMU is on, convert every linked data address to physical.
    ldr x0, =__stack_top
    ldr x1, =__kernel_virt_base
    sub x0, x0, x1
    mov sp, x0

    mrs x2, CurrentEL
    lsr x2, x2, #2
    cmp x2, #2
    b.ne 4f

    // EL2 (QEMU -kernel default): enable FP for EL1, then drop to EL1h.
    msr cptr_el2, xzr
    mov x2, #0x300000
    msr cpacr_el1, x2
    mov x2, #(1 << 31)
    msr hcr_el2, x2
    mov x2, #0x3c5
    msr spsr_el2, x2
    adr x2, 4f
    msr elr_el2, x2
    eret
4:
    mov x2, #0x300000
    msr cpacr_el1, x2
    isb

    // Zero .bss through the low physical alias. The boot/final page tables
    // live there too; no Rust code may run before the high map is active.
    ldr x3, =__bss_start
    ldr x4, =__bss_end
    ldr x5, =__kernel_virt_base
    sub x3, x3, x5
    sub x4, x4, x5
    cmp x3, x4
    b.hs 3f
2:
    str xzr, [x3], #8
    cmp x3, x4
    b.lo 2b
3:
    // TABLES layout (see mmu.rs):
    //   +0     low TTBR0 L0 (identity map during this boot stub only)
    //   +4096  high TTBR1 L0
    //   +8192  shared L1: MMIO gigabyte + DRAM gigabyte
    //   +12288 MMIO L2
    //   +16384 DRAM L2
    // Both L0[0] entries share the same L1, producing low and high aliases.
    ldr x5, =TABLES
    ldr x6, =__kernel_virt_base
    sub x5, x5, x6
    add x6, x5, #4096
    add x7, x5, #8192
    add x8, x5, #12288
    add x9, x5, #16384

    orr x10, x7, #3
    str x10, [x5]
    str x10, [x6]
    orr x10, x8, #3
    str x10, [x7]
    orr x10, x9, #3
    str x10, [x7, #8]

    // 512 x 2 MB blocks per gigabyte. Attr1 device-nGnRnE for MMIO,
    // attr0 normal WB inner-shareable for DRAM; AP=EL1_RW, AF=1, UXN=1.
    mov x11, #0
    mov x12, #512
    mov x13, #0x405
    movk x13, #0x40, lsl #48
    mov x14, #0x701
    movk x14, #0x40, lsl #48
    mov x15, #0x40000000
1:
    lsl x16, x11, #21
    orr x17, x16, x13
    str x17, [x8, x11, lsl #3]
    add x17, x16, x15
    orr x17, x17, x14
    str x17, [x9, x11, lsl #3]
    add x11, x11, #1
    cmp x11, x12
    b.ne 1b

    // 48-bit VAs on both halves, 4 KB granules, 40-bit PAs.
    mov x0, #0xff
    msr mair_el1, x0
    ldr x0, =0x0000000200103510
    msr tcr_el1, x0
    msr ttbr0_el1, x5
    msr ttbr1_el1, x6
    isb
    mrs x0, sctlr_el1
    orr x0, x0, #1
    orr x0, x0, #(1 << 22)  // PAN on: user access requires LDTR/STTR
    msr sctlr_el1, x0
    isb
    tlbi vmalle1
    dsb ish
    isb

    // The identity alias is still live here, so the next fetch is safe.
    // Jump to the linked high PC and never execute kernel code low again.
    ldr x0, =5f
    br x0
5:
    ldr x0, =__stack_top
    mov sp, x0
    ldr x1, =exception_vectors
    msr vbar_el1, x1
    isb
    bl kernel_main
1:
    wfe
    b 1b
    .size _start, . - _start
"#
);

#[cfg(all(not(feature = "std"), target_arch = "aarch64"))]
core::arch::global_asm!(
    r#"
    .section .text.vectors, "ax"
    .align 11
    .global exception_vectors
    .type exception_vectors, %object
exception_vectors:
    b exception_sync_entry
    .align 7
    b exception_irq_entry
    .align 7
    b exception_fiq_entry
    .align 7
    b exception_serror_entry

    .align 7
    b exception_sync_entry
    .align 7
    b exception_irq_entry
    .align 7
    b exception_fiq_entry
    .align 7
    b exception_serror_entry

    .align 7
    b exception_sync_entry
    .align 7
    b exception_irq_entry
    .align 7
    b exception_fiq_entry
    .align 7
    b exception_serror_entry

    .align 7
    b exception_sync_entry
    .align 7
    b exception_irq_entry
    .align 7
    b exception_fiq_entry
    .align 7
    b exception_serror_entry

exception_sync_entry:
    msr daifset, #2
    // ESR decoding needs scratch registers, but x9/x10 still belong to the
    // user. Preserve them before the SVC check so optimized userspace can
    // safely keep live values in any register across svc #0.
    sub sp, sp, #16
    stp x9, x10, [sp]
    mrs x9, esr_el1
    lsr x10, x9, #26
    cmp x10, #21             // EC == 0b010101: SVC from AArch64
    b.eq exception_sync_svc
    ldp x9, x10, [sp]
    add sp, sp, #16
    // Anything else: if it came from EL0 it is a task fault (bad pointer,
    // bad instruction, trapped MSR...) — kill the task, keep the kernel.
    // If it came from EL1 it is a kernel bug — panic as before.
    mrs x11, spsr_el1
    and x11, x11, #0xC       // SPSR.M[3:2]: 0b00 = EL0t
    cbz x11, exception_user_fault
    mov x0, #0
    b exception_dispatch
exception_sync_svc:
    ldp x9, x10, [sp]
    add sp, sp, #16
    b exception_svc_dispatch
exception_user_fault:
    mrs x0, esr_el1
    mrs x1, elr_el1
    mrs x2, far_el1
    b silver_handle_task_fault
exception_irq_entry:
    b exception_irq_dispatch
exception_fiq_entry:
    mov x0, #2
    b exception_dispatch
exception_serror_entry:
    mov x0, #3
    b exception_dispatch

// System call trampoline. Frame on the kernel stack (272 bytes, 16-aligned):
//   [sp + 0..248)  x0-x30
//   [sp + 248]     ELR_EL1   (return address after the svc instruction)
//   [sp + 256]     SPSR_EL1
// ELR/SPSR are saved on the STACK (not trusted to stay in the CPU registers):
// a syscall may block and switch to another task whose exceptions would
// otherwise overwrite the CPU-global ELR_EL1/SPSR_EL1 mid-syscall.
//
// A context switch resumes this task via `ret`: it restores x19-x30/SP and
// returns into schedule(), which returns here with SP still pointing at this
// frame — so the epilogue below always finds its ELR/SPSR slots intact.
exception_svc_dispatch:
    // Mask IRQs for the whole syscall: the handlers manipulate scheduler
    // state (block, wake, schedule). eret restores the caller's PSTATE
    // from the frame.
    msr daifset, #2
    sub sp, sp, #272
    stp x0, x1, [sp, #0]
    stp x2, x3, [sp, #16]
    stp x4, x5, [sp, #32]
    stp x6, x7, [sp, #48]
    stp x8, x9, [sp, #64]
    stp x10, x11, [sp, #80]
    stp x12, x13, [sp, #96]
    stp x14, x15, [sp, #112]
    stp x16, x17, [sp, #128]
    stp x18, x19, [sp, #144]
    stp x20, x21, [sp, #160]
    stp x22, x23, [sp, #176]
    stp x24, x25, [sp, #192]
    stp x26, x27, [sp, #208]
    stp x28, x29, [sp, #224]
    str x30, [sp, #240]
    mrs x9, elr_el1
    str x9, [sp, #248]
    mrs x9, spsr_el1
    str x9, [sp, #256]
    mov x0, sp
    bl silver_handle_syscall
    // Fall through to the epilogue: the handler may have switched this task
    // away and back (the switch resumes via `ret`, unwinding the call chain
    // to this exact point), so the frame and SP are exactly as we left them.
exception_svc_epilogue:
    // Restore ELR/SPSR first, while x9 is still trampoline scratch.
    ldr x9, [sp, #248]
    msr elr_el1, x9
    ldr x9, [sp, #256]
    msr spsr_el1, x9
    ldr x30, [sp, #240]
    ldp x28, x29, [sp, #224]
    ldp x26, x27, [sp, #208]
    ldp x24, x25, [sp, #192]
    ldp x22, x23, [sp, #176]
    ldp x20, x21, [sp, #160]
    ldp x18, x19, [sp, #144]
    ldp x16, x17, [sp, #128]
    ldp x14, x15, [sp, #112]
    ldp x12, x13, [sp, #96]
    ldp x10, x11, [sp, #80]
    ldp x8, x9, [sp, #64]
    ldp x6, x7, [sp, #48]
    ldp x4, x5, [sp, #32]
    ldp x2, x3, [sp, #16]
    ldp x0, x1, [sp, #0]
    add sp, sp, #272
    eret

exception_irq_dispatch:
    // Mask IRQs before touching anything: the handler calls schedule(),
    // and a nested timer IRQ here would re-enter the scheduler mid-switch
    // and corrupt task contexts. eret restores the pre-interrupt PSTATE
    // from the frame.
    msr daifset, #2
    // Frame is 272 bytes like the SVC trampoline: x0-x30, then ELR, SPSR.
    // ELR_EL1/SPSR_EL1 MUST be saved and restored here — schedule() may
    // switch to another task whose exceptions overwrite these CPU-global
    // registers; without the stack copy, our eret would return into the
    // wrong task at the wrong place.
    sub sp, sp, #272
    stp x0, x1, [sp, #0]
    stp x2, x3, [sp, #16]
    stp x4, x5, [sp, #32]
    stp x6, x7, [sp, #48]
    stp x8, x9, [sp, #64]
    stp x10, x11, [sp, #80]
    stp x12, x13, [sp, #96]
    stp x14, x15, [sp, #112]
    stp x16, x17, [sp, #128]
    stp x18, x19, [sp, #144]
    stp x20, x21, [sp, #160]
    stp x22, x23, [sp, #176]
    stp x24, x25, [sp, #192]
    stp x26, x27, [sp, #208]
    stp x28, x29, [sp, #224]
    str x30, [sp, #240]
    mrs x9, elr_el1
    str x9, [sp, #248]
    mrs x9, spsr_el1
    str x9, [sp, #256]
    bl silver_handle_irq
    // Fall through to the epilogue (see the SVC trampoline above).
exception_irq_epilogue:
    ldr x9, [sp, #248]
    msr elr_el1, x9
    ldr x9, [sp, #256]
    msr spsr_el1, x9
    ldr x30, [sp, #240]
    ldp x28, x29, [sp, #224]
    ldp x26, x27, [sp, #208]
    ldp x24, x25, [sp, #192]
    ldp x22, x23, [sp, #176]
    ldp x20, x21, [sp, #160]
    ldp x18, x19, [sp, #144]
    ldp x16, x17, [sp, #128]
    ldp x14, x15, [sp, #112]
    ldp x12, x13, [sp, #96]
    ldp x10, x11, [sp, #80]
    ldp x8, x9, [sp, #64]
    ldp x6, x7, [sp, #48]
    ldp x4, x5, [sp, #32]
    ldp x2, x3, [sp, #16]
    ldp x0, x1, [sp, #0]
    add sp, sp, #272
    eret

exception_dispatch:
    mrs x8, CurrentEL
    lsr x8, x8, #2
    cmp x8, #2
    b.ne 1f
    mrs x1, esr_el2
    mrs x2, elr_el2
    mrs x3, far_el2
    b silver_handle_exception
1:
    mrs x1, esr_el1
    mrs x2, elr_el1
    mrs x3, far_el1
    b silver_handle_exception
"#
);

#[cfg(not(feature = "std"))]
#[no_mangle]
pub extern "C" fn kernel_main() -> ! {
    mmu::init();

    let memory_map = memory::current();
    let available_pages = frame_allocator::init(memory_map.available);

    pl011::init();
    pl011::write_str("MMU=OK\r\n");

    if !memory_map.is_valid() || available_pages == 0 || !frame_allocator::run_self_test() {
        exceptions::silver_handle_exception(6, 0, 0, 0);
    }

    let Some(heap_base) = allocator::init() else {
        exceptions::silver_handle_exception(7, 0, 0, 0);
    };

    let value = alloc::boxed::Box::new(42_u64);
    let value_address = (&*value as *const u64) as usize;

    pl011::write_str("HEAP=OK BASE=");
    pl011::write_hex_u64(heap_base as u64);
    pl011::write_str(" VALUE=");
    pl011::write_hex_u64(*value);
    pl011::write_str(" ADDR=");
    pl011::write_hex_u64(value_address as u64);
    pl011::write_str("\r\n");

    drop(value);

    gic::init();
    timer::init();
    vfs::init();
    scheduler::init();

    pl011::write_str("IRQ READY\r\n");

    for &byte in b"HELLO, WORLD!\r\n" {
        pl011::write_byte(byte);
    }

    pl011::write_str("SCHEDULER START\r\n");
    scheduler::start();
}

#[cfg(not(feature = "std"))]
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    if let Some(location) = info.location() {
        pl011::write_str("PANIC AT ");
        pl011::write_str(location.file());
        pl011::write_str(":");
        pl011::write_dec_u64(location.line() as u64);
        pl011::write_str("\r\n");
    }
    if let Some(message) = info.message().as_str() {
        pl011::write_str("PANIC MSG ");
        pl011::write_str(message);
        pl011::write_str("\r\n");
    }
    exceptions::silver_handle_exception(4, 0, 0, 0)
}

#[cfg(feature = "std")]
fn main() {}

#![cfg_attr(not(feature = "std"), no_std)]
#![cfg_attr(not(feature = "std"), no_main)]

#[cfg(not(feature = "std"))]
extern crate alloc;

#[cfg(not(feature = "std"))]
mod allocator;
#[cfg(not(feature = "std"))]
mod condvar;
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
mod mutex;
#[cfg(not(feature = "std"))]
mod nameserver;
#[cfg(not(feature = "std"))]
mod pl011;
#[cfg(not(feature = "std"))]
mod scheduler;
#[cfg(not(feature = "std"))]
mod syscall;
#[cfg(not(feature = "std"))]
mod timer;

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
    ldr x0, =__stack_top
    mov sp, x0

    mrs x2, CurrentEL
    lsr x2, x2, #2
    cmp x2, #2
    b.ne 4f
    msr cptr_el2, xzr
    mov x2, #0x300000
    msr cpacr_el1, x2
    b 5f
4:
    mov x2, #0x300000
    msr cpacr_el1, x2
5:
    isb

    ldr x3, =__bss_start
    ldr x4, =__bss_end
    cmp x3, x4
    b.hs 3f
2:
    str xzr, [x3], #8
    cmp x3, x4
    b.lo 2b
3:
    ldr x1, =exception_vectors
    mrs x2, CurrentEL
    lsr x2, x2, #2
    cmp x2, #2
    b.ne 1f
    msr vbar_el2, x1
    isb
    b 2f
1:
    msr vbar_el1, x1
    isb
2:
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
    mrs x9, esr_el1
    lsr x10, x9, #26
    cmp x10, #21             // EC == 0b010101: SVC from AArch64
    b.eq exception_svc_dispatch
    mov x0, #0
    b exception_dispatch
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
exception_svc_dispatch:
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
    sub sp, sp, #256
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
    bl silver_handle_irq
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
    add sp, sp, #256
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
    let memory_map = memory::current();
    let available_pages = frame_allocator::init(memory_map.available);

    pl011::init();

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
    scheduler::init();

    pl011::write_str("IRQ READY\r\n");

    for &byte in b"HELLO, WORLD!\n" {
        pl011::write_byte(byte);
    }

    pl011::write_str("SCHEDULER START\r\n");
    scheduler::start();
}

#[cfg(not(feature = "std"))]
#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    exceptions::silver_handle_exception(4, 0, 0, 0)
}

#[cfg(feature = "std")]
fn main() {}

#![cfg_attr(not(feature = "std"), no_std)]
#![cfg_attr(not(feature = "std"), no_main)]

#[cfg(not(feature = "std"))]
extern crate alloc;

#[cfg(not(feature = "std"))]
mod allocator;
#[cfg(not(feature = "std"))]
mod boot_metrics;
#[cfg(not(feature = "std"))]
mod capability;
#[cfg(not(feature = "std"))]
mod channel;
#[cfg(not(feature = "std"))]
mod condvar;
#[cfg(not(feature = "std"))]
mod console;
#[cfg(not(feature = "std"))]
mod elf;
#[cfg(not(feature = "std"))]
mod elf_loader;
#[cfg(not(feature = "std"))]
mod exceptions;
#[cfg(not(feature = "std"))]
mod frame_allocator;
#[cfg(not(feature = "std"))]
mod geni_uart;
#[cfg(not(feature = "std"))]
mod gic;
mod gicv3;
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
mod platform;
mod ram_log;
#[cfg(not(feature = "std"))]
mod scheduler;
#[cfg(not(feature = "std"))]
mod seed;
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
// RAM-log boot markers, tablet only: with the MMU off every write is Device
// memory, so a store to the unassigned 0xA2200000 would abort on QEMU. The
// assembler flag RAM_LOG_MARKS (set in the tiny global_asm below) switches the
// marker code in the boot stub at assembly time.
#[cfg(feature = "tablet")]
core::arch::global_asm!(".set RAM_LOG_MARKS, 1");
#[cfg(not(feature = "tablet"))]
core::arch::global_asm!(".set RAM_LOG_MARKS, 0");

core::arch::global_asm!(
    r#"
    .section .text.boot, "ax"
    .global _head
    // arm64 Image header: ABL (UEFI-style) looks for the "ARM\\x64" magic at
    // 0x38 and image_size at 0x10 before it will decompress/jump. Without it
    // a raw objcopy binary is not recognized and the device lands in the
    // Qualcomm crashdump. code0 branches over the 64-byte header into _start.
    // NOTE: the kernel is NOT self-relocating (linked at 0xA8000000); the boot
    // marker records the actual load PC into the pmsg ramoops zone so a wrong
    // load address can be read back from Android and the kernel relinked.
    _head:
        b       1f                          // 0x00 code0: jump past header
        .word   0                           // 0x04 code1
        .quad   0                           // 0x08 text_offset: GRUB jumps to payload start (Linux convention)
        .quad   __kernel_end - _head        // 0x10 image_size
        .quad   0                           // 0x18 flags
        .quad   0                           // 0x20 reserved
        .quad   0                           // 0x28 reserved
        .quad   0                           // 0x30 reserved
        .word   0x644D5241                  // 0x38 magic "ARM\\x64"
        .word   0                           // 0x3C reserved
    .global _start
    .type _start, %function
1:
_start:
    // Secondary cores park FIRST: on the tablet eight cores may come out of
    // ABL running, and QEMU starts only one. Any secondary executing this
    // stub would zero .bss twice and scribble over the page tables; a wfe
    // loop waits for the (never yet implemented) PSCI_CPU_ON wake-up.
    mrs x10, mpidr_el1
    ubfx x10, x10, #0, #24    // Aff0/Aff1/Aff2: CPU0 is 0x0 in the device tree
    cmp x10, #0
    b.ne 6f

    // --- K0: PAINT — the very first observable action ----------------------
    // Fill the top 64 lines of all three 16 MiB frame slots inside the
    // cont_splash region with white. MMU is off: physical stores, VA == PA.
    // If ANY of the three appears on screen, the payload executes and the
    // panel scans out that slot; silence means GRUB never jumped into us.
.if RAM_LOG_MARKS
    ldr x18, =0xD5100000        // frame 0
    bl 19f
    ldr x18, =0xD60B0000        // frame 1
    bl 19f
    ldr x18, =0xD7060000        // frame 2
    bl 19f
    b 20f
19: mov w15, #0xFFF            // white, low byte 0xFFF&0xFF... build 0xFFFFFFFF
    movk w15, #0xFFFF, lsl #16
    mov x16, #64               // 64 rows
    mov x17, #6400             // stride bytes (1600*4)
1:  mov x14, x18
    mov x13, #400              // 1600 px / 4 px per word? 1600*4B = 6400B = 800 words
    mov x13, #800
2:  str w15, [x14], #4
    sub x13, x13, #1
    cbnz x13, 2b
    add x18, x18, x17
    sub x16, x16, #1
    cbnz x16, 1b
    ret
20:
.endif

    // --- K1: MEASURE before touching anything -----------------------------
    // adr gives the real PC (the address ABL actually loaded us at, which may
    // differ from the link address); SCTLR is recorded raw so we can see
    // whether the MMU or caches were still on at handover.
    adr x9, .
    mrs x10, CurrentEL
    mrs x11, sctlr_el1

    // --- K2: NORMALIZE: MMU + D-cache + I-cache off at the current EL -----
    // Every physical store below assumes VA == PA. Note the measurement above
    // happens first: if markers now show up, x11 tells us the handover state.
    mrs x13, CurrentEL
    lsr x13, x13, #2
    cmp x13, #2
    b.eq 10f
    bic x14, x11, #(1 << 12)  // I
    bic x14, x14, #(1 << 2)   // C
    bic x14, x14, #1          // M
    msr sctlr_el1, x14
    b 11f
10:
    mrs x14, sctlr_el2
    bic x14, x14, #(1 << 12)
    bic x14, x14, #(1 << 2)
    bic x14, x14, #1
    msr sctlr_el2, x14
    bic x14, x11, #(1 << 12)
    bic x14, x14, #(1 << 2)
    bic x14, x14, #1
    msr sctlr_el1, x14
11:
    dsb sy
    tlbi vmalle1
    ic iallu
    dsb sy
    isb

    // --- K3: RECORD the measurement into both ramoops zones ---------------
    // Live layout (module params + the 4 MiB hole in /proc/iomem):
    // console 0xA2000000..0xA2200000, pmsg 0xA2200000..0xA2400000. Both get a
    // valid persistent_ram header so pstore exposes the block verbatim:
    //   +0x20 M0: PC at entry   +0x28 M1: CurrentEL   +0x30 M2: SCTLR raw
    //   +0x38 M3: zero after .bss zeroing   +0x40 M4: zero after tables
    .macro PROBE base
    ldr x18, =\base
    ldr x19, =0x43474239        // persistent_ram sig
    str x19, [x18]
    str wzr, [x18, #4]          // start = 0
    mov w19, #0x48
    str w19, [x18, #8]          // size: header + M0..M4
    str x9,  [x18, #0x20]       // M0: PC
    str x10, [x18, #0x28]       // M1: CurrentEL
    str x11, [x18, #0x30]       // M2: SCTLR_EL1 at entry
    dc cvac, x18
    dsb sy
    .endm

.if RAM_LOG_MARKS
    PROBE 0xA2000000
    PROBE 0xA2200000
.endif


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

    // EL2 (QEMU -kernel default; ABL may also hand over here): enable FP for
    // EL1, route timer control to EL1, then drop to EL1h with the mini
    // vectors installed so any abort before mmu::init lands in the ramoops
    // log instead of going to EL3 as an anonymous QCDUMP.
    msr cptr_el2, xzr
    mov x2, #0x300000
    msr cpacr_el1, x2
    mov x2, #(1 << 31)      // HCR_EL2.RW: EL1 is AArch64
    msr hcr_el2, x2
    mov x2, #3              // CNTHCTL_EL2: EL1PCVEN|EL1PCTEN
    msr cnthctl_el2, x2
    ldr x2, =0x30C50838     // initial SCTLR_EL1 (MMU off, caches per spec)
    msr sctlr_el1, x2
    // VBAR_EL1 = mini_vectors at its PHYSICAL address (MMU is off here).
    ldr x2, =mini_vectors
    ldr x3, =__kernel_virt_base
    sub x2, x2, x3
    msr vbar_el1, x2
    msr vbar_el2, x2        // aborts taken at EL2 (before the eret) too
    isb
    mov x2, #0x3c5
    msr spsr_el2, x2
    adr x2, 4f
    msr elr_el2, x2
    eret
4:
    // Mini vectors first, on BOTH exception levels the entry may run at, so
    // any abort before mmu::init is logged into the ramoops pmsg zone instead
    // of vanishing into an EL3 QCDUMP.
    ldr x11, =mini_vectors
    ldr x12, =__kernel_virt_base
    sub x11, x11, x12       // physical address: MMU is off (or identity)
    msr vbar_el1, x11
    mrs x2, CurrentEL
    lsr x2, x2, #2
    cmp x2, #2
    b.ne 5f
    msr vbar_el2, x11
5:
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
.if RAM_LOG_MARKS
    // M3: .bss (and the page tables inside it) are zeroed.
    ldr x18, =0xA2000000
    str xzr, [x18, #0x38]
    ldr x18, =0xA2200000
    str xzr, [x18, #0x38]
    dc cvac, x18
    dsb sy
.endif
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

    ldr x15, =__dram_base

    orr x10, x7, #3
    str x10, [x5]
    str x10, [x6]
    orr x10, x8, #3
    str x10, [x7]
    // DRAM L1 slot = __dram_base >> 30 (identity alias at the right index).
    lsr x16, x15, #30
    lsl x16, x16, #3
    orr x10, x9, #3
    str x10, [x7, x16]

    // 512 x 2 MB blocks per gigabyte. Attr1 device-nGnRnE for MMIO,
    // attr0 normal WB inner-shareable for DRAM; AP=EL1_RW, AF=1, UXN=1.
    mov x11, #0
    mov x12, #512
    mov x13, #0x405
    movk x13, #0x40, lsl #48
    mov x14, #0x701
    movk x14, #0x40, lsl #48
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

.if RAM_LOG_MARKS
    // M4: the boot page tables are built and the MMU is on; the zones are now
    // reachable only through the high alias. Clean to the PoC.
    ldr x18, =0xFFFF0000A2000000
    str xzr, [x18, #0x40]
    dc cvac, x18
    ldr x18, =0xFFFF0000A2200000
    str xzr, [x18, #0x40]
    dc cvac, x18
    dsb sy
.endif

    // The identity alias is still live here, so the next fetch is safe.
    // Jump to the linked high PC and never execute kernel code low again.
    ldr x0, =8f
    br x0
8:
    ldr x0, =__stack_top
    mov sp, x0
    ldr x1, =exception_vectors
    msr vbar_el1, x1
    isb
    bl kernel_main
1:
    wfe
    b 1b
6:
    // Secondary core parking lot: never woken up yet (PSCI_CPU_ON is not
    // implemented); any spurious event just loops back to sleep.
    wfe
    b 6b

    // Mini exception vectors: active from the EL2 drop until mmu::init
    // installs the real vector table. No stack, position-independent, MMU
    // state unknown — the handler only stores registers into the ramoops
    // console zone (physical address, VA==PA with the MMU off) and halts.
    .balign 2048
    .global mini_vectors
mini_vectors:
    .rept 16
    b mini_handler
    .space 124 // each vector slot is 128 bytes
    .endr

mini_handler:
    mrs x8, CurrentEL
    mrs x9, esr_el1
    mrs x10, far_el1
    mrs x11, elr_el1
    ldr x12, =0xA2040000        // ramoops pmsg zone (from the device DTB)
    ldr x13, =0x43474239        // persistent_ram sig
    str x13, [x12]
    str wzr, [x12, #4]          // start = 0
    mov w13, #0x100             // size = 256 bytes of evidence
    str w13, [x12, #8]
    ldr x13, =0x21435845        // 'E','X','C','!' little-endian: text "EXC!" at +0x40
    str w13, [x12, #0x40]
    str x8, [x12, #0x48]        // CurrentEL
    str x9, [x12, #0x50]        // ESR_EL1
    str x10, [x12, #0x58]       // FAR_EL1
    str x11, [x12, #0x60]       // ELR_EL1
    dc cvac, x12
    dsb sy
9:
    wfe
    b 9b
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
    // The tablet has no readable UART, so EVERY console/boot_metrics call is
    // compiled out there: a stray write to a QEMU address (PL011 0x09000000,
    // GICv2 0x08000000) would be a bus abort -> QCDUMP before any log. On the
    // tablet the only output channel is the ramoops RAM log.
    #[cfg(not(feature = "tablet"))]
    boot_metrics::mark_boot_start();

    // The ramoops console zone lives in reserved DRAM; on the tablet it is the
    // only log channel, on QEMU ram_log is a no-op (the UART is the channel).
    ram_log::init();
    ram_log::log("KERNEL ENTRY");
    ram_log::fb_progress();

    mmu::init();

    let memory_map = memory::current();
    let available_pages = frame_allocator::init(memory_map.available);

    #[cfg(not(feature = "tablet"))]
    {
        console::init();
        console::write_str("PLATFORM=QEMU\r\n");
        console::write_str("MMU=OK\r\n");
        boot_metrics::log_milestone("MMU init");
    }
    #[cfg(feature = "tablet")]
    console::init(); // GENI UART at 0x89C000, inside the mapped low gigabyte
    ram_log::log("MMU OK");
    ram_log::fb_progress();

    if !memory_map.is_valid() || available_pages == 0 || !frame_allocator::run_self_test() {
        exceptions::silver_handle_exception(6, 0, 0, 0);
    }
    ram_log::log("FRAME ALLOC OK");
    ram_log::fb_progress();

    let Some(heap_base) = allocator::init() else {
        exceptions::silver_handle_exception(7, 0, 0, 0);
    };

    let value = alloc::boxed::Box::new(42_u64);
    #[cfg(not(feature = "tablet"))]
    {
        let value_address = (&*value as *const u64) as usize;
        console::write_str("HEAP=OK BASE=");
        console::write_hex_u64(heap_base as u64);
        console::write_str(" VALUE=");
        console::write_hex_u64(*value);
        console::write_str(" ADDR=");
        console::write_hex_u64(value_address as u64);
        console::write_str("\r\n");
        boot_metrics::log_milestone("Heap init");
    }
    ram_log::log("HEAP OK");
    ram_log::fb_progress();

    drop(value);

    // The tablet path shares the QEMU boot sequence from here on: gic::init
    // dispatches to gicv3, the timer PPI is INTID 30 on both platforms, and
    // scheduler::init unconditionally unmasks IRQs (daifclr #2).
    gic::init();
    timer::init();
    ram_log::log("GICv3+TIMER OK");
    ram_log::fb_progress();
    vfs::init();
    seed::seed();
    ram_log::log("VFS+SEED OK");
    ram_log::fb_progress();
    scheduler::init();

    #[cfg(not(feature = "tablet"))]
    {
        console::write_str("IRQ READY\r\n");
        for &byte in b"HELLO, WORLD!\r\n" {
            console::write_byte(byte);
        }
        boot_metrics::log_milestone("Scheduler start");
        console::write_str("SCHEDULER START\r\n");
    }
    ram_log::log("SCHEDULER START - BOOT COMPLETE");
    ram_log::fb_fill(0, 120, 0); // full green = fully booted
    scheduler::start();
}

#[cfg(not(feature = "std"))]
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // Tablet: no console output at all — the line number and message go into
    // the ramoops log; a stray UART write here could abort before logging.
    #[cfg(feature = "tablet")]
    {
        crate::ram_log::log("PANIC");
        if let Some(location) = info.location() {
            crate::ram_log::log_raw(" line=");
            crate::ram_log::log_hex(location.line() as u64);
            crate::ram_log::log_raw("\n");
        }
    }
    #[cfg(not(feature = "tablet"))]
    {
        if let Some(location) = info.location() {
            console::write_str("PANIC AT ");
            console::write_str(location.file());
            console::write_str(":");
            console::write_dec_u64(location.line() as u64);
            console::write_str("\r\n");
        }
        if let Some(message) = info.message().as_str() {
            console::write_str("PANIC MSG ");
            console::write_str(message);
            console::write_str("\r\n");
        }
    }
    exceptions::silver_handle_exception(4, 0, 0, 0)
}

#[cfg(feature = "std")]
fn main() {}

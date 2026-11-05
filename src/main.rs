#![cfg_attr(not(feature = "std"), no_std)]
#![cfg_attr(not(feature = "std"), no_main)]

#[cfg(not(feature = "std"))]
extern crate alloc;

#[cfg(not(feature = "std"))]
mod allocator;
#[cfg(not(feature = "std"))]
mod boot;
#[cfg(not(feature = "std"))]
mod boot_metrics;
#[cfg(not(feature = "std"))]
mod cache;
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
mod fdt;
#[cfg(not(feature = "std"))]
mod fog;
#[cfg(all(not(feature = "std"), feature = "fog-diag"))]
mod gic_diag;
#[cfg(all(not(feature = "std"), feature = "fog-ui"))]
mod fog_ui;
mod fog_validate;
#[cfg(not(feature = "std"))]
mod frame_allocator;
#[cfg(not(feature = "std"))]
mod geni_uart;
#[cfg(not(feature = "std"))]
mod gic;
mod gicv3;
mod gicv3_policy;
mod irq_policy;
#[cfg(not(feature = "std"))]
mod interrupts;
#[cfg(not(feature = "std"))]
mod ipc;
#[cfg(not(feature = "std"))]
mod kernel_stack;
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
// The boot stub mirrors this policy in assembly; the Rust copy exists for
// host boundary tests, so the binary itself does not call these items.
#[allow(dead_code)]
mod reloc;
#[cfg(not(feature = "std"))]
mod scheduler;
#[cfg(not(feature = "std"))]
mod seed;
#[cfg(not(feature = "std"))]
mod semaphore;
mod smp;
#[cfg(not(feature = "std"))]
mod syscall;
#[cfg(not(feature = "std"))]
mod timer;
#[cfg(not(feature = "std"))]
mod user_memory;
#[cfg(not(feature = "std"))]
mod user_program;
#[cfg(all(not(feature = "std"), feature = "smp"))]
mod watchdog;
#[cfg(not(feature = "std"))]
mod vfs;
#[cfg(not(feature = "std"))]
mod wait_queue;

#[cfg(not(feature = "std"))]
use core::panic::PanicInfo;

/// C-ABI entry point called from the pre-relocation boot stub. Kept in the
/// binary (not the shared library) so the host-testable module does not emit
/// a second global symbol when Cargo links both crates.
#[no_mangle]
#[inline(never)]
pub unsafe extern "C" fn fog_validate_relocation(
    dtb: *const u8,
    dst_start: u64,
    dst_end: u64,
    src_start: u64,
    src_end: u64,
) -> u64 {
    fog_validate::validate_relocation(dtb, dst_start, dst_end, src_start, src_end)
}

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
// Fog images are linked for one physical address. Standard fog builds do not
// move the image: if ABL placed it elsewhere the stub publishes the measured
// pc/link/delta and parks, because only the live FDT can prove a destination
// free. `fog-reloc` is the explicit opt-in for the copy path.
#[cfg(feature = "fog")]
core::arch::global_asm!(".set FOG_BUILD, 1");
#[cfg(not(feature = "fog"))]
core::arch::global_asm!(".set FOG_BUILD, 0");
#[cfg(feature = "fog-reloc")]
core::arch::global_asm!(".set FOG_RELOC, 1");
#[cfg(not(feature = "fog-reloc"))]
core::arch::global_asm!(".set FOG_RELOC, 0");
// Debug-only visual evidence on the Rain/Fog cont_splash framebuffer. The
// address comes from the live Rain reserved-memory tree, not from a generic
// fog fact; only enable it for a controlled diagnostic boot.
#[cfg(feature = "fog-paint")]
core::arch::global_asm!(".set FOG_PAINT, 1");
#[cfg(not(feature = "fog-paint"))]
core::arch::global_asm!(".set FOG_PAINT, 0");

core::arch::global_asm!(
    r#"
    // Debug-only full-buffer fill. MMU is off, so this is a physical store
    // into the Rain/Fog cont_splash region. It is never enabled in a normal
    // build; the color ladder is the only output channel on a UART-less unit.
    .macro FOG_FILL start, end, lo, hi
    ldr x18, =\start
    ldr x19, =\end
    mov w20, #\lo
    movk w20, #\hi, lsl #16
.Lfog_fill_loop\@:
    str w20, [x18], #4
    cmp x18, x19
    b.lo .Lfog_fill_loop\@
    .endm

    .section .text.boot, "ax"
    .global _head
    // arm64 Image header: ABL (UEFI-style) looks for the "ARM\\x64" magic at
    // 0x38 and image_size at 0x10 before it will decompress/jump. Without it
    // a raw objcopy binary is not recognized and the device lands in the
    // Qualcomm crashdump. code0 branches over the 64-byte header into _start.
    // The tablet/QEMU images are not self-relocating (they are linked at
    // 0xA8000000 / 0x4008_0000). The fog stub records the observed hand-over
    // PC in BOOT_PARAMS and, by default, parks on a mismatch; the explicit
    // `fog-reloc` feature enables the checked copy to __kernel_link_pa.
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
    // Kimi step 1: mask IRQ/FIQ/SError first — the firmware handover may
    // leave pending exceptions that would fire into a not-yet-set VBAR.
    msr daifset, #0xf

    // Preserve the two values the C/Rust side can never recover once the
    // stub starts using x0: the bootloader's DTB pointer and the address the
    // image was actually loaded at (PC-relative, so it works at any LMA).
    mov x22, x0
    adr x23, _head

    // Claim the boot path. The bootloader normally starts a single CPU, but
    // its MPIDR affinity is not guaranteed to be zero (an unconfirmed
    // parameter on fog). Whichever core arrives first claims the image; every
    // other core that sees the claim parks instead of zeroing .bss and the
    // page tables twice.
    //
    // Plain LDR/STR, not LDAXR/STXR: with the MMU off the access is Device
    // memory on real hardware, where exclusive monitors are not available.
    // The claim is therefore best-effort and the hand-over is expected to
    // start one core.
    adrp x10, {boot_claim}
    add x10, x10, :lo12:{boot_claim}
    mov w11, #0x4F54
    movk w11, #0x424F, lsl #16      // "BOOT"
    ldr w12, [x10]
    cmp w12, w11
    b.eq 6f
    str w11, [x10]

.if FOG_PAINT
    // Green: the payload executed at least to the claimed boot path.
    FOG_FILL 0x5C000000, 0x5C800000, 0xFF00, 0xFF00
.endif

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
19: mov w15, #0xFF00          // GREEN band (Kimi step 7): 0xFF00FF00
    movk w15, #0xFF00, lsl #16
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

    // --- FOG: check the placement, relocate only when explicitly enabled --
    // ABL chooses the placement; the linker chooses the address all absolute
    // references were built for. The default fog build does NOT copy: a
    // destination in DRAM is not proof that the bytes are free. The live FDT
    // can reject the reservations it describes, but it is not a complete
    // ownership proof: the fixed 0x40080000 slot is safe only under a platform
    // boot contract that says ABL/firmware/DMA do not use it. On a mismatch
    // the stub publishes pc/link/delta and parks before touching anything
    // outside its own image.
    //
    // With `fog-reloc` the copy is still fail-closed: header magic, image
    // size, both span ends, overflow and every overlap are checked first.
.if FOG_BUILD
.if FOG_RELOC == 0
    mov x24, #0                     // 1 = entered at a non-link address
.endif
    ldr x13, =__kernel_link_pa
    cmp x23, x13
    b.eq .Lfog_at_link
.if FOG_RELOC
    // image_size is read from the running image's own header. The read is
    // safe even before the magic check: the source image is owned by us.
    ldr x14, [x23, #0x10]

    // Diagnostic record in the running source image's .data, written before
    // any destination access. A refused relocation therefore still leaves
    // evidence in DRAM; a successful copy carries the same record with it.
    // x25 = record, w24 = passed checks, w27 = reason, w28 = flags.
    // x24/x25/x27/x28 are callee-saved and survive the FDT validator call.
    ldr x25, ={reloc_record}
    ldr x26, =__kernel_virt_base
    sub x25, x25, x26               // link-time physical address
    ldr x26, =__kernel_link_pa
    sub x26, x23, x26               // actual - link
    add x25, x25, x26               // record in the source image
    mov w24, #0
    mov w27, #0
    mov w28, #1                     // RELOC_FLAG_ATTEMPTED
    ldr w26, =0x32524C53            // "SLR2"
    str w26, [x25, #0x00]
    mov w26, #2
    strb w26, [x25, #0x04]
    lsr w26, w10, #2
    strb w26, [x25, #0x05]
    strh w28, [x25, #0x06]
    strh w27, [x25, #0x08]
    strh w24, [x25, #0x0A]
    str wzr, [x25, #0x0C]
    str x23, [x25, #0x10]
    str x13, [x25, #0x18]
    str xzr, [x25, #0x20]           // dest_end unknown until the span check
    str x14, [x25, #0x28]
    str x22, [x25, #0x30]
    str xzr, [x25, #0x38]
    // x11 is the K1 entry read. Reaching this point means that read already
    // completed, so at EL1 it also proves HCR_EL2.TRVM=0 for this boot.
    str x11, [x25, #0x40]
    orr w28, w28, #(1 << 2)         // SCTLR_EL1 read completed at K1
    mrs x26, mpidr_el1
    str x26, [x25, #0x58]
    // HCR_EL2/CTR_EL0 reads are only unconditional at EL2. At EL1 they are
    // TID2/TRVM-dependent; leave the flags clear instead of adding a new trap.
    lsr w26, w10, #2
    cmp w26, #2
    b.ne 41f
    mrs x26, hcr_el2
    str x26, [x25, #0x50]
    orr w28, w28, #(1 << 4)         // HCR_READ
    mrs x26, ctr_el0
    str x26, [x25, #0x48]
    orr w28, w28, #(1 << 3)         // CTR_READ
41:
    // DTB size: bounded check first; a wild x0 must not fault.
    cbz x22, 42f
    ldr x26, =0x40000000
    cmp x22, x26
    b.lo 42f
    ldr x26, =0x7FFFFFD8            // DRAM_END - 40-byte FDT header
    cmp x22, x26
    b.hi 42f
    ldr w26, [x22, #4]
    rev w26, w26
    str x26, [x25, #0x38]
    orr w28, w28, #(1 << 5)         // DTB_PRESENT
42:
    strh w28, [x25, #0x06]

    // Recognise the ARM64 Image header before trusting any field in it. A
    // stripped payload, a wrong entry point or a corrupted image must not turn
    // into a wild copy of an arbitrary length.
    mov w27, #2                     // RELOC_REASON_BAD_MAGIC
    ldr w15, [x23, #0x38]           // magic at _head + 0x38
    ldr w16, =0x644D5241            // "ARM\x64" in little-endian memory
    cmp w15, w16
    b.ne .Lfog_park
    orr w24, w24, #(1 << 0)         // RELOC_CHECK_MAGIC

    mov w27, #3                     // RELOC_REASON_BAD_SIZE
    cmp x14, #0x1000
    b.lo .Lfog_park                 // smaller than one page: refuse
    mov x15, #0x4000000             // 64 MiB upper bound
    cmp x14, x15
    b.hs .Lfog_park
    tst x14, #7
    b.ne .Lfog_park                 // the copy loop moves 8 bytes at a time
    orr w24, w24, #(1 << 1)         // RELOC_CHECK_SIZE

    // The destination is a build-time constant, not a measured fact. Reject
    // a wild value before the write: page-aligned and inside the mapped first
    // DRAM gigabyte.
    mov w27, #4                     // RELOC_REASON_BAD_ALIGN
    tst x13, #0xFFF
    b.ne .Lfog_park
    orr w24, w24, #(1 << 2)         // RELOC_CHECK_ALIGN

    // Overflow checks for both spans before any range comparison, so a
    // wrapped end can never look plausible.
    mov w27, #5                     // RELOC_REASON_BOUNDS
    adds x15, x23, x14              // x15 = src_end
    b.cs .Lfog_park
    adds x16, x13, x14              // x16 = dst_end
    b.cs .Lfog_park
    str x16, [x25, #0x20]           // dest_end is now known

    // Check BOTH ends of the destination: a valid start whose end leaves the
    // mapped gigabyte is refused.
    ldr x17, =0x40000000
    cmp x13, x17
    b.lo .Lfog_park
    ldr x17, =0x80000000
    cmp x13, x17
    b.hs .Lfog_park
    cmp x16, x17
    b.hi .Lfog_park

    // The source must fit the same mapped window.
    ldr x17, =0x40000000
    cmp x23, x17
    b.lo .Lfog_park
    ldr x17, =0x80000000
    cmp x23, x17
    b.hs .Lfog_park
    cmp x15, x17
    b.hi .Lfog_park
    orr w24, w24, #(1 << 3)         // RELOC_CHECK_BOUNDS

    // Only a completely non-overlapping move is allowed. Any overlap can
    // overwrite the running loop or bytes between the spans that this stub
    // cannot prove dead. Adjacent spans are accepted.
    mov w27, #6                     // RELOC_REASON_OVERLAP
    cmp x13, x23
    b.hs 47f                        // dest > src
    // dest < src: require dest_end <= src.
    cmp x16, x23
    b.hi .Lfog_park
    b .Lfog_fdt_check
47: // dest > src: require dest >= src_end.
    cmp x13, x15
    b.lo .Lfog_park

.Lfog_fdt_check:
    orr w24, w24, #(1 << 4)         // RELOC_CHECK_OVERLAP
    // The arithmetic checks above prove only that the copy loop cannot
    // clobber itself. They do not prove that the destination bytes are free.
    // Read the live FDT now, while the image is still at the address ABL
    // chose: the validator accepts only a span inside /memory and outside
    // every reservation, the DTB and the running source image.
    //
    // The call needs a stack, so point SP at this image's own stack through
    // the actual (pre-relocation) load address. x9/x10/x11 are the measured
    // hand-over values and must survive the C call.
    ldr x0, =__stack_top
    ldr x1, =__kernel_virt_base
    sub x0, x0, x1                  // link-time physical stack top
    ldr x1, =__kernel_link_pa
    sub x1, x23, x1                 // actual - link
    add x0, x0, x1                  // actual physical stack top
    mov sp, x0

    stp x9, x10, [sp, #-16]!
    str x11, [sp, #-16]!
    mov x0, x22                     // live DTB pointer from x0
    mov x1, x13                     // destination start
    mov x2, x16                     // destination end
    mov x3, x23                     // source start
    mov x4, x15                     // source end
    mov w27, #7                     // RELOC_REASON_FDT_REJECT
    bl fog_validate_relocation
    ldr x11, [sp], #16
    ldp x9, x10, [sp], #16
    cbnz x0, .Lfog_fdt_ok
    b .Lfog_park

.Lfog_fdt_ok:
    orr w24, w24, #(1 << 5)         // RELOC_CHECK_FDT
    // The call clobbered the caller-saved range checks; reload the two
    // values the copy loop itself needs.
    ldr x13, =__kernel_link_pa
    ldr x14, [x23, #0x10]

    // Cache-maintenance contract for this copy:
    //
    // * With SCTLR_EL1.M=0, data accesses are Device-nGnRnE unless the code
    //   runs at EL1/EL0 with HCR_EL2.DC=1; then they are Normal, Non-shareable,
    //   Write-Back Write-Allocate. SCTLR_EL1.C=0 alone is NOT the reason for
    //   the memory type. The copy may therefore be non-cacheable (EL2, or
    //   EL1 with DC=0) or cacheable (EL1 with DC=1).
    // * DC IVAC at EL1 is a pure invalidate only when HCR_EL2.VM=0. With
    //   VM=1 it behaves as DC CIVAC (clean+invalidate); at EL2 it is a pure
    //   invalidate. The pre-copy pass is safe for an owned destination in
    //   both cases: stale lines are removed before the new bytes are written.
    // * The post-copy DC CVAC is kept because it is required when the copy
    //   stores are cacheable (EL1 with HCR_EL2.DC=1, or a future cacheable
    //   mapping). When the stores are non-cacheable it is a no-op.
    // * IC IALLU is per-PE. This boot path is single-core: APs are parked and
    //   SMP is disabled. A future SMP path needs IC IALLUIS/broadcast and a
    //   DSB ISH before another PE fetches the copy.
    //
    // EL1 prerequisites: HCR_EL2.TPC (DC IVAC/CVAC), TPU (IC IALLU) and
    // TID2 (CTR_EL0) must be 0. The K1 entry SCTLR read additionally needs
    // HCR_EL2.TRVM=0; K2's SCTLR write needs TVM=0. F6 clears these only on
    // an EL2 hand-over and only later; an EL1 hand-over relies on the
    // platform boot contract. On FEAT_FGT implementations the equivalent
    // HFGITR_EL2/HFGRTR_EL2 bits can trap the same operations even with the
    // HCR bits clear.
    //
    // Ownership: the FDT validator is necessary but not sufficient. The fixed
    // 0x40080000 slot additionally requires the platform boot contract.
    //
    // The line size comes from CTR_EL0.DminLine (bits [19:16]); the
    // architectural maximum is 2 KiB (CTR_EL0.CWG max 512 words), so page
    // alignment is sufficient for the line-aligned start invariant.
    //
    // This CTR_EL0 read is required by the cache loop, not added for
    // diagnostics. At EL1 it completes only when HCR_EL2.TID2=0; the record
    // stores the raw value and sets CTR_READ only after the read returned.
    mrs x16, ctr_el0
    str x16, [x25, #0x48]           // raw CTR_EL0 for the diagnostic record
    orr w28, w28, #(1 << 3)         // RELOC_FLAG_CTR_READ
    strh w28, [x25, #0x06]
    ubfx x16, x16, #16, #4
    mov x17, #4
    lsl x17, x17, x16           // D-cache line size in bytes
    sub x18, x17, #1
    add x16, x13, x14           // destination end
    bic x15, x13, x18           // line-aligned start (page-aligned here)
49:
    dc ivac, x15
    add x15, x15, x17
    cmp x15, x16
    b.lo 49b
    dsb sy

.Lfog_copy:
    // Non-overlapping forward copy. Both directions are covered above.
    mov x15, #0
.Lfog_copy_loop:
    ldr x16, [x23, x15]
    str x16, [x13, x15]
    add x15, x15, #8
    cmp x15, x14
    b.lo .Lfog_copy_loop
    // Publish success into the destination copy of the record. This code is
    // still executing from the source image, so compute the link address
    // explicitly instead of using a PC-relative reference.
    ldr x26, ={reloc_record}
    ldr x21, =__kernel_virt_base
    sub x26, x26, x21
    mov w27, #0                     // RELOC_REASON_NONE
    orr w28, w28, #(1 << 1)         // RELOC_FLAG_DONE
    strh w27, [x26, #0x08]
    strh w24, [x26, #0x0A]
    strh w28, [x26, #0x06]
    // The copy has written a new instruction stream. Complete those stores,
    // then clean to the point of coherency: a no-op while they are
    // non-cacheable (EL2, or EL1 with HCR_EL2.DC=0), but required when they
    // are cacheable (EL1 with DC=1, or a future cacheable mapping). Then
    // invalidate the I-cache before fetching from the copy. Both loops use
    // the same line size and range.
    dsb sy
    add x16, x13, x14           // destination end
    bic x15, x13, x18
50:
    dc cvac, x15
    add x15, x15, x17
    cmp x15, x16
    b.lo 50b
    dsb sy
    ic iallu
    dsb sy
    isb
    // Continue at the same offset inside the copy.
    adr x15, .Lfog_at_link
    sub x15, x15, x23
    add x15, x13, x15
    br x15

.Lfog_park:
    strh w27, [x25, #0x08]         // reason
    strh w24, [x25, #0x0A]         // checks passed
    strh w28, [x25, #0x06]         // flags
    wfe
    b .Lfog_park                   // unsafe relocation: refuse to corrupt memory
.else
    mov x24, #1                     // diagnosed and parked after the record
.endif
.Lfog_at_link:
.if FOG_PAINT
    // Blue: the placement policy passed (at-link or the checked copy ran).
    FOG_FILL 0x5C000000, 0x5C800000, 0x00FF, 0xFF00
.endif
.endif

    // --- Publish the hand-over record for `boot::get()`. -------------------
    // After relocation the PC is at the link address, so an ordinary
    // PC-relative symbol reference names the mapped physical address.
    adrp x3, {boot_params}
    add x3, x3, :lo12:{boot_params}
    ldr x4, =0x30315245564C4953     // "SILVER10"
    str x4, [x3, #0x00]
    str x23, [x3, #0x08]            // actual_pc
    str x10, [x3, #0x10]            // CurrentEL at entry
    str x11, [x3, #0x18]            // SCTLR_EL1 at entry
    mrs x4, mpidr_el1
    str x4, [x3, #0x20]
    str x22, [x3, #0x28]            // dtb_pa from x0
    ldr x4, =__kernel_link_pa
    str x4, [x3, #0x30]             // image_base
    cbz x22, 47f
    ldr w5, [x22, #4]               // DTB totalsize (big-endian)
    rev w5, w5
    str x5, [x3, #0x38]
    b 48f
47: str xzr, [x3, #0x38]
48:
    adr x5, _head
    ldr x5, [x5, #0x10]             // image_size (relocated header)
    str x5, [x3, #0x40]
    sub x5, x23, x4
    str x5, [x3, #0x48]             // load_delta (i64)

    // Default fog build, non-link placement: the diagnostics above are now
    // stored inside this image's own .data, so park before any further memory
    // is touched. Absolute references would address the link slot, not this
    // copy, so continuing would execute against the wrong bytes.
.if FOG_BUILD
.if FOG_RELOC == 0
    cbz x24, .Lfog_diag_done
.Lfog_diag_park:
    wfe
    b .Lfog_diag_park
.Lfog_diag_done:
.endif
.endif

    // --- K3: RECORD the measurement into both ramoops zones ---------------
    // Live layout (module params + the 4 MiB hole in /proc/iomem):
    // console 0xA2000000..0xA2200000, pmsg 0xA2200000..0xA2400000. Both get a
    // valid persistent_ram header so pstore exposes the block verbatim:
    //   +0x20 M0: PC at entry   +0x28 M1: CurrentEL   +0x30 M2: SCTLR raw
    //   +0x38 M3: zero after .bss zeroing   +0x40 M4: zero after tables
    //   +0x48 DTB pointer from the handover (x0; 0 on QEMU)
    //   +0x50 DTB totalsize (BE u32 from the DTB header, host order here)
    .macro PROBE base
    ldr x18, =\base
    ldr x19, =0x43474239        // persistent_ram sig
    str x19, [x18]
    str wzr, [x18, #4]          // start = 0
    mov w19, #0x58
    str w19, [x18, #8]          // size: header + M0..M4 + DTB ptr + DTB size
    str x9,  [x18, #0x20]       // M0: PC
    str x10, [x18, #0x28]       // M1: CurrentEL
    str x11, [x18, #0x30]       // M2: SCTLR_EL1 at entry
    str x0,  [x18, #0x48]       // Kimi step 5: DTB pointer from linuxdirect
    dc cvac, x18
    dsb sy
    .endm

.if RAM_LOG_MARKS
    PROBE 0xA2000000
    PROBE 0xA2200000
.endif

    // --- Kimi steps 2-4: stack, image_size, full-image cache flush ---------
    // The final kernel stack lives in the image's NOLOAD .stack section,
    // after .bss; the zeroing loop below therefore never touches it. Reach it
    // through the low physical alias (the MMU is still off here).
    ldr x0, =__stack_top
    ldr x1, =__kernel_virt_base
    sub x0, x0, x1
    mov sp, x0

    adr x12, _start
    ldr x13, [x12, #-0x30]      // image_size @ _head+0x10 = _start-0x30
    mrs x15, ctr_el0
    ubfx x15, x15, #16, #4
    mov x14, #4
    lsl x14, x14, x15           // D-cache line size
    add x15, x12, x13           // end = _start + image_size
1:  dc cvac, x12                // Kimi step 4: clean the whole image
    add x12, x12, x14
    cmp x12, x15
    b.lo 1b
    dsb sy
    ic iallu
    dsb sy
    isb

    // --- Kimi step 6: M0 ASCII stamp "SILVER PC=XXXXXXXX EL=n\n" -----------
    // Written after the PROBE header (sig/start/size/M0-M2/DTB are already
    // in place) into both zones, as printable text in console-ramoops-0 and
    // pmsg-ramoops-0. x9/x10 are copies: PROBE stored the raw values above.
.if RAM_LOG_MARKS
    mov x20, x9
    mov x21, x10
    .macro M0TXT base, pc, el
    ldr x18, =\base
    movz x13, #0x4953           // "SILVER P" (8 bytes, LE)
    movk x13, #0x564C, lsl #16
    movk x13, #0x5245, lsl #32
    movk x13, #0x5020, lsl #48
    str x13, [x18, #0x20]
    movz w13, #0x3D43           // "C="
    str w13, [x18, #0x28]
    mov x16, #8
    add x15, x18, #0x2A         // hex digits start here
22: lsr x13, \pc, #60
    cmp x13, #10
    blt 23f
    add x13, x13, #55           // 'A'-10
    b 24f
23: add x13, x13, #48           // '0'
24: strb w13, [x15], #1
    lsl \pc, \pc, #4
    sub x16, x16, #1
    cbnz x16, 22b
    movz w13, #0x4520           // " EL=" (LE: 20 45 4C 3D)
    movk w13, #0x3D4C, lsl #16
    str w13, [x15]
    add x15, x15, #4
    lsr x13, \el, #2
    add x13, x13, #48
    strb w13, [x15], #1
    mov w13, #10                // '\n'
    strb w13, [x15], #1
    mov x14, x18                // clean the stamped 0x80 bytes to PoC
    add x13, x18, #0x80
25: dc cvac, x14
    add x14, x14, #64
    cmp x14, x13
    b.lo 25b
    dsb sy
    .endm
    M0TXT 0xA2000000, x20, x21
    M0TXT 0xA2200000, x20, x21
.endif

    // --- Kimi step 6: DTB — copy to fixed physical scratch 0xA4000000 -----
    // kexec purgatory (and ABL) pass the physical DTB in x0. BSS is
    // linked-address dependent and wrong until the relink, so the copy goes
    // to fixed free DRAM instead; it works no matter where we were loaded.
    // The original x0 pointer is already in the marker zones (+0x48).
.if RAM_LOG_MARKS
    cbz x0, 27f                 // no DTB (QEMU -kernel): skip
    ldr w13, [x0, #4]
    rev w13, w13                // DTB totalsize: big-endian u32 at +4
    // A zero, tiny or unaligned totalsize would make the copy below wrap or
    // read past the source. This is device-controlled input before any FDT
    // validation, so fail closed (skip the copy) instead of trusting it.
    cbz x13, 27f
    cmp x13, #8
    b.lo 27f
    tst x13, #3
    b.ne 27f
    mov x14, #0xA400
    lsl x14, x14, #16           // dst = 0xA4000000 (free DRAM, nobody's)
    mov x15, x0                 // src
    mov x16, x14
    mov x17, x13
    mov x12, #0x100
    lsl x12, x12, #20           // 1 MiB cap
    cmp x17, x12
    b.ls 26f
    mov x17, x12
26: ldr w11, [x15], #4
    str w11, [x16], #4
    sub x17, x17, #4
    cbnz x17, 26b
    ldr x18, =0xA2000000        // record totalsize at +0x50 of both zones
    str w13, [x18, #0x50]
    dc cvac, x18
    ldr x18, =0xA2200000
    str w13, [x18, #0x50]
    dc cvac, x18
    dsb sy
27:
.endif

    mrs x2, CurrentEL
    lsr x2, x2, #2
    cmp x2, #2
    b.ne 4f

    // EL2 (QEMU -kernel default; ABL may also hand over here): enable FP for
    // EL1, route timer control to EL1, then drop to EL1h with the mini
    // vectors installed so any abort before mmu::init lands in the ramoops
    // log instead of going to EL3 as an anonymous QCDUMP.
    //
    // CPTR_EL2: clear only the traps that would break this EL1 kernel:
    //   TCPAC (31) traps EL1 CPACR_EL1 writes,
    //   TTA   (20) traps trace accesses,
    //   TFP   (10) traps FP/SIMD.
    // Every other CPA/CPTR control the firmware set is preserved.
    mrs x2, cptr_el2
    ldr x3, =0x80100400        // TCPAC | TTA | TFP
    bic x2, x2, x3
    msr cptr_el2, x2
    mov x2, #0x300000
    msr cpacr_el1, x2
    // HCR_EL2 is deliberately owned by the kernel, not merged. EL1&0 needs
    // E2H=0, TGE=0, virtual interrupt routing off and every EL1 trap bit
    // clear (WFI/WFE, TLBI, cache maintenance, TTBR/TCR/SCTLR, ID registers,
    // CPACR, SMC). Preserving an ABL trap bit here would trap those
    // operations to EL2 and hang the kernel; a safe "preserve unrelated
    // bits" merge needs an explicit architecture-level allow-list first.
    mov x2, #(1 << 31)      // HCR_EL2.RW: EL1 is AArch64
    msr hcr_el2, x2
    // CNTHCTL_EL2: allow EL1 access to the physical timer and counter, but
    // keep event-stream and other unrelated bits the firmware programmed.
    mrs x2, cnthctl_el2
    orr x2, x2, #3              // EL1PCEN | EL1PCTEN
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
    // The ramoops marker is only safe on the tablet, where 0xA2040000 is a
    // known reserved DRAM zone. On fog/QEMU that address is not proven to be
    // RAM at all, so an early abort must park without touching memory.
.if RAM_LOG_MARKS
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
.endif
9:
    wfe
    b 9b
    .size _start, . - _start
"#,
    boot_params = sym crate::boot::BOOT_PARAMS,
    boot_claim = sym crate::boot::BOOT_CLAIM,
    reloc_record = sym crate::boot::RELOC_RECORD
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
    // BISECT probes: 18 = dispatch entered, 17 = handler returned.
    // x9/x10 are only scratch here: every user register is already saved in
    // the frame above, and the epilogue restores x0-x30 from it.
    adrp x9, {probe}
    mov w10, #18
    str w10, [x9, #:lo12:{probe}]
    mrs x9, elr_el1
    str x9, [sp, #248]
    mrs x9, spsr_el1
    str x9, [sp, #256]
    bl silver_handle_irq
    adrp x9, {probe}
    mov w10, #17
    str w10, [x9, #:lo12:{probe}]
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
"#,
    probe = sym crate::smp::SMP_PROBE
);

#[cfg(not(feature = "std"))]
#[no_mangle]
pub extern "C" fn kernel_main() -> ! {
    // Per-CPU identity first: every scheduler/lock path reads TPIDR_EL1.
    smp::init_cpu(0);

    // Fog: prove the machine from the live FDT before any device is touched.
    // `probe` only reads; it publishes the platform identity, the UART base
    // and one FDT-derived frame-allocator window. If the tree is missing or
    // not a khaje/SM6225 board, nothing is forced and the boot path fails
    // closed instead of inventing a map.
    #[cfg(feature = "fog")]
    let boot_params = boot::get().unwrap_or(boot::BootParams::zero());
    #[cfg(feature = "fog")]
    let fog_info = fog::probe(&boot_params);
    #[cfg(feature = "fog")]
    if fog_info.valid {
        memory::set_fog_available(
            fog_info.memory.start as usize,
            fog_info.memory.end as usize,
        );
    }

    // The tablet has no readable UART, so EVERY console/boot_metrics call is
    // compiled out there: a stray write to a QEMU address (PL011 0x09000000,
    // GICv2 0x08000000) would be a bus abort -> QCDUMP before any log. On the
    // tablet the only output channel is the ramoops RAM log.
    #[cfg(all(not(feature = "tablet"), not(feature = "fog")))]
    boot_metrics::mark_boot_start();

    // The ramoops console zone lives in reserved DRAM; on the tablet it is the
    // only log channel, on QEMU ram_log is a no-op (the UART is the channel).
    ram_log::init();
    ram_log::log("KERNEL ENTRY");

    mmu::init();
    #[cfg(feature = "fog-paint")]
    fog::paint(0xFF00_FFFF); // cyan: MMU is on
    ram_log::fb_progress(); // entry bar — after mmu::init so the fb is mapped

    // Console first: a bad memory map must be *reported* before the
    // allocator asserts on an empty window.
    console::init();
    // Cache state is reported on every console-capable profile. The boot
    // self-test below re-checks the same bits on the APs, whose values are
    // recorded per CPU rather than inferred from this line.
    #[cfg(not(feature = "tablet"))]
    {
        console::write_str("CACHE=");
        if cache::caches_on(cache::read_sctlr()) {
            console::write_str("ON line=");
            console::write_dec_u64(cache::line_bytes() as u64);
        } else {
            console::write_str("OFF");
        }
        console::write_str("\r\n");
    }
    #[cfg(feature = "fog-paint")]
    fog::paint(0xFFFF_FF00); // yellow: console/probe path is alive
    #[cfg(all(not(feature = "tablet"), not(feature = "fog")))]
    {
        console::write_str("PLATFORM=QEMU\r\n");
        console::write_str("MMU=OK\r\n");
        boot_metrics::log_milestone("MMU init");
    }

    // Size AP bring-up from the live tree instead of assuming MAX_CPUS. QEMU
    // passes a DTB even with `-kernel`; fog/tablet stay single-core by policy
    // and a missing tree leaves the old default in place.
    #[cfg(all(not(feature = "tablet"), not(feature = "fog")))]
    {
        let params = boot::get();
        let dtb = params.as_ref().and_then(fog::dtb_slice);
        let mut parsed = 0usize;
        if let Some(data) = dtb {
            if let Some(tree) = fdt::Fdt::new(data) {
                parsed = tree.cpu_count(smp::MAX_CPUS);
                smp::set_possible_cpus(parsed);
            }
        }
        // QEMU `-kernel` passes no DTB, so this is evidence only when a
        // bootloader did hand one over (and the count is non-zero).
        if parsed > 0 && cfg!(feature = "smp-selftest") {
            console::write_str("SMP: fdt cpus=");
            console::write_dec_u64(parsed as u64);
            console::write_str("\r\n");
        }
    }

    #[cfg(feature = "fog")]
    {
        fog::log_banner(&boot_params, &fog_info);
        console::write_str("SILVER MMU OK\r\n");
        if !fog_info.valid || fog_info.memory.is_empty() {
            console::write_str("SILVER FOG HALT: no proven frame-allocator window\r\n");
            loop {
                core::hint::spin_loop();
            }
        }
    }
    ram_log::log("MMU OK");
    ram_log::fb_progress();

    let memory_map = memory::current();
    if !memory_map.is_valid() || memory_map.available.is_empty() {
        #[cfg(feature = "fog")]
        console::write_str("SILVER FOG HALT: invalid memory map\r\n");
        exceptions::silver_handle_exception(6, 0, 0, 0);
    }
    let available_pages = frame_allocator::init(memory_map.available);

    if !memory_map.is_valid() || available_pages == 0 || !frame_allocator::run_self_test() {
        exceptions::silver_handle_exception(6, 0, 0, 0);
    }
    ram_log::log("FRAME ALLOC OK");
    ram_log::fb_progress();

    // User-visible memory (mmap, brk, DMA, ELF images, task stacks) may not
    // eat the frames the kernel and its devices still need. The gate is a
    // free-count policy; prove here that it refuses one-sidedly and that the
    // raw kernel allocator is unaffected.
    if !frame_allocator::run_user_reserve_selftest() {
        exceptions::silver_handle_exception(6, 0, 0, 0);
    }
    ram_log::log("MEM RESERVE OK");
    ram_log::fb_progress();

    let Some(heap_base) = allocator::init() else {
        exceptions::silver_handle_exception(7, 0, 0, 0);
    };

    let value = alloc::boxed::Box::new(42_u64);
    #[cfg(all(not(feature = "tablet"), not(feature = "fog")))]
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

    // One 2 MiB-aligned arena, split into 4 KiB pages with one unmapped
    // guard page below every task stack. A failure here would make task
    // spawns silently unsafe, so it stops the boot.
    if !kernel_stack::init() || !kernel_stack::run_self_test() {
        exceptions::silver_handle_exception(6, 0, 0, 0);
    }
    ram_log::log("KERNEL STACK OK");
    ram_log::fb_progress();

    // Address-space reclamation invariants: partial unmap is refused without
    // touching the mapped page, teardown retires the root token once, and a
    // second teardown with the same token is refused.
    if !mmu::run_self_test() {
        exceptions::silver_handle_exception(9, 0, 0, 0);
    }
    ram_log::log("MMU RECLAIM OK");
    ram_log::fb_progress();

    // TLB proof: read through TTBR0, remap the same VA to another frame and
    // read again. Fails closed if the flush did not change the translation.
    if !mmu::run_tlb_self_test() {
        exceptions::silver_handle_exception(9, 0, 0, 0);
    }
    ram_log::log("MMU TLB OK");
    ram_log::fb_progress();

    // ASID isolation: two spaces map the same VA to different frames; two
    // switches without any flush must still return each space's own value.
    if !mmu::run_asid_self_test() {
        exceptions::silver_handle_exception(9, 0, 0, 0);
    }
    console::write_str("MMU ASID OK\r\n");
    ram_log::log("MMU ASID OK");
    ram_log::fb_progress();

    // Heap-L3 persistence: emptying the heap through the generic unmap must
    // not free the table, and the next heap map must land in the same slot
    // instead of writing through a zero descriptor.
    if !mmu::run_heap_table_self_test() {
        exceptions::silver_handle_exception(9, 0, 0, 0);
    }
    console::write_str("MMU HEAP OK\r\n");
    ram_log::log("MMU HEAP OK");
    ram_log::fb_progress();

    // Recycled DMA frames must be zero before EL0 can read them.
    if !mmu::run_dma_zero_self_test() {
        exceptions::silver_handle_exception(9, 0, 0, 0);
    }
    console::write_str("DMA ZERO OK\r\n");
    ram_log::log("DMA ZERO OK");
    ram_log::fb_progress();

    // RX ring arithmetic under head/tail wraparound (the cross-CPU exclusion
    // is the lock around push/pop; this proves the indices themselves).
    #[cfg(all(feature = "smp", feature = "smp-selftest"))]
    {
        if !pl011::run_ring_self_test() {
            exceptions::silver_handle_exception(9, 0, 0, 0);
        }
        console::write_str("UART RING OK\r\n");
        ram_log::log("UART RING OK");
        ram_log::fb_progress();

        // SYS_STOP_CONSOLE requires CAP_CONSOLE_GRANT: the owner has it, a
        // plain driver does not.
        if !scheduler::run_console_cap_selftest() {
            exceptions::silver_handle_exception(9, 0, 0, 0);
        }
        console::write_str("CONSOLE CAP OK\r\n");
        ram_log::log("CONSOLE CAP OK");
        ram_log::fb_progress();
    }

    // DMA quarantine window: a released run must survive the grace passes and
    // then return to the frame allocator.
    if !scheduler::run_dma_quarantine_self_test() {
        exceptions::silver_handle_exception(9, 0, 0, 0);
    }
    ram_log::log("DMA QUARANTINE OK");
    ram_log::fb_progress();

    // In-flight DMA lease gate: an armed device range must block reuse until
    // the driver drops the lease.
    if !scheduler::run_dma_lease_self_test() {
        exceptions::silver_handle_exception(9, 0, 0, 0);
    }
    console::write_str("DMA LEASE OK\r\n");
    ram_log::log("DMA LEASE OK");
    ram_log::fb_progress();

    drop(value);
    #[cfg(feature = "fog")]
    {
        console::write_str("SILVER FRAME ALLOCATOR OK\r\nSILVER HEAP OK base=0x");
        console::write_hex_u64(heap_base as u64);
        console::write_str("\r\n");
    }
    #[cfg(feature = "fog-paint")]
    fog::paint(0xFFFF_00FF); // magenta: frame allocator + heap are alive

    // The tablet/fog path shares the QEMU boot sequence from here on: gic::init
    // dispatches to gicv3, and the timer PPI is selected per platform (INTID
    // 30 on SM8650, 18 on SM6225/khaje).
    gic::init();
    #[cfg(feature = "fog-diag")]
    crate::gic_diag::capture(0); // before the timer is armed
    timer::init();
    #[cfg(feature = "fog-diag")]
    crate::gic_diag::capture(1); // after the timer is armed
    // BISECT: isolated timer test — unmask IRQs in kernel context, wait for
    // ticks only when a redistributor was confirmed. Without one, spinning
    // for an interrupt that cannot be delivered would just hide the failure.
    #[cfg(any(not(feature = "fog"), feature = "fog-timer-test"))]
    {
        #[cfg(feature = "fog-paint")]
        fog::paint(0xFFFF_FF80); // pale yellow: entering the timer test
        if timer::available() {
            let saved = interrupts::save_and_disable_irq();
            interrupts::enable_irq();
            let mut spin: u32 = 0;
            while timer::ticks() < 30 && spin < 800_000_000 {
                spin += 1;
                core::hint::spin_loop();
            }
            interrupts::restore_irq(saved);
            console::write_str("TIMER TEST: ticks=");
            console::write_dec_u64(timer::ticks());
            console::write_str(" spins=");
            console::write_dec_u64(spin as u64);
            console::write_str("\r\n");
        } else {
            console::write_str("TIMER TEST: SKIP (no confirmed GICv3 timer PPI)\r\n");
        }
    }
    #[cfg(all(feature = "fog", not(feature = "fog-timer-test")))]
    {
        // The first hardware boot must not deliver a timer IRQ before
        // scheduler::init_fog(): gicv3::handle_irq calls schedule() on every
        // tick, and the scheduler state is not ready yet.
        console::write_str("TIMER TEST: SKIP (fog hardware default)\r\n");
    }
    #[cfg(feature = "fog-paint")]
    fog::paint(0xFF00_FF80); // spring green: timer test finished/skipped
    ram_log::log("GICv3+TIMER OK");
    ram_log::fb_progress();
    vfs::init();
    seed::seed();
    #[cfg(all(not(feature = "tablet"), not(feature = "fog")))]
    vfs::run_self_test();
    ram_log::log("VFS+SEED OK");
    ram_log::fb_progress();

    #[cfg(feature = "fog")]
    {
        console::write_str("SILVER PLATFORM READY\r\n");
        scheduler::init_fog();
        console::write_str("SILVER SCHEDULER START\r\n");
        #[cfg(feature = "fog-paint")]
        fog::paint(0xFFFF_FFFF); // white: scheduler is about to start
        scheduler::start();
    }
    #[cfg(not(feature = "fog"))]
    {
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

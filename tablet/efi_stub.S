// efi_stub.S — PE32+/EFI wrapper around the Silver kernel Image.
//
// Loaded via GRUB `chainloader /Image.efi` + `boot` (StartImage), this stub:
//   0. paints milestone bands into the splash framebuffer (0xD5100000, the
//      buffer the panel scans out) so progress is VISIBLE with no UEFI
//      protocol dependency. Bands (top of screen, 32 rows each):
//        GREEN   (y=0)   : entry
//        WHITE   (y=32)  : kernel copied to 0xA8000000 + cache cleaned
//        MAGENTA (y=64)  : running at EL2
//        CYAN    (y=64)  : running at EL1
//        YELLOW  (y=96)  : MMU successfully disabled
//   1. verifies SystemTable signature, prints "SILVER BOOT" via ConOut;
//   2. copies the embedded kernel to its link address 0xA8000000;
//   3. disables the MMU (and D-cache) at the current EL — PC-relative
//      `b` + `isb` immediately after each `msr sctlr` so the post-MMU-off
//      fetch re-reads a predictable physical address;
//   4. paints YELLOW (MMU off done), then `br 0xA8000000` into the kernel.
//
// pmsg marker write was REMOVED: pmsg-ramoops-0 never appears on this device,
// so the marker was unreadable; the kernel's own `_start` records markers.
//
// Built with clang (COFF) + lld-link (ImageBase=0).

    .text
    .globl  efi_entry

efi_entry:
    mov     x19, x1                     // save SystemTable (x19 = callee-saved)

    // --- 0. Entry: GREEN ---------------------------------------------------
    mov     x0, #0
    mov     w1, #0xFF00
    movk    w1, #0xFF00, lsl #16        // 0xFF00FF00 green
    bl      milestone

    // --- 1. Verify SystemTable->Hdr.Signature == "IBI SYST" ----------------
    ldr     x2, [x19]
    movz    x3, #0x4249
    movk    x3, #0x5320, lsl #16
    movk    x3, #0x5359, lsl #32
    movk    x3, #0x5453, lsl #48
    cmp     x2, x3
    b.ne    sig_bad

    // --- 2. Print "SILVER BOOT\r\n" via ConOut->OutputString ---------------
    ldr     x2, [x19, #0x40]
    cbz     x2, sig_bad
    ldr     x3, [x2, #0x08]
    cbz     x3, sig_bad
    adr     x1, boot_msg
    mov     x0, x2
    blr     x3

sig_bad:
    // --- 3. Measure (before MMU off) --------------------------------------
    adr     x9, .                       // x9 = real load PC
    mrs     x10, CurrentEL
    mrs     x11, sctlr_el1

    // --- 4. Copy the embedded kernel to its link address 0xA8000000 --------
    adrp    x0, kernel_start
    add     x0, x0, :lo12:kernel_start
    adrp    x1, kernel_end
    add     x1, x1, :lo12:kernel_end
    movz    x2, #0xA800, lsl #16        // dest = 0xA8000000
    mov     x3, x0
    mov     x4, x2
copy_loop:
    cmp     x3, x1
    b.hs    copy_done
    ldr     x5, [x3], #8
    str     x5, [x4], #8
    b       copy_loop
copy_done:

    // --- 5. Clean the copied region to PoC --------------------------------
    sub     x1, x1, x0
    add     x1, x1, x2
    mrs     x15, ctr_el0
    ubfx    x15, x15, #16, #4
    mov     x6, #4
    lsl     x6, x6, x15
    mov     x0, x2
clean_loop:
    dc      cvac, x0
    add     x0, x0, x6
    cmp     x0, x1
    b.lo    clean_loop
    dsb     sy

    // --- 5b. Milestone: WHITE (copy + clean done) --------------------------
    mov     x0, #32
    mov     w1, #0xFFFF
    movk    w1, #0xFFFF, lsl #16        // 0xFFFFFFFF white
    bl      milestone

    // --- 6. EL check, band, and MMU-off -----------------------------------
    msr     daifset, #0xf               // mask async exceptions first
    mrs     x13, CurrentEL
    lsr     x13, x13, #2
    cmp     x13, #2
    b.eq    el2_path

    // EL1: cyan band, then disable MMU + D-cache via SCTLR_EL1.
    mov     x0, #64
    mov     w1, #0xFFFF
    movk    w1, #0xFF00, lsl #16        // 0xFF00FFFF cyan
    bl      milestone
    mrs     x14, sctlr_el1
    bic     x14, x14, #(1 << 2)         // C
    bic     x14, x14, #1                // M
    msr     sctlr_el1, x14
    isb
    b       mmu_off_done

el2_path:
    // EL2: magenta band, then disable MMU + D-cache via SCTLR_EL2.
    mov     x0, #64
    mov     w1, #0x00FF
    movk    w1, #0xFFFF, lsl #16        // 0xFFFF00FF magenta
    bl      milestone
    mrs     x14, sctlr_el2
    bic     x14, x14, #(1 << 2)         // C
    bic     x14, x14, #1                // M
    msr     sctlr_el2, x14
    isb
    b       mmu_off_done

mmu_off_done:
    // --- 7. Milestone: YELLOW (MMU disabled) ------------------------------
    mov     x0, #96
    mov     w1, #0xFF00
    movk    w1, #0xFFFF, lsl #16        // 0xFFFFFF00 yellow
    bl      milestone

    // --- 8. Jump into the kernel at 0xA8000000 -----------------------------
    movz    x0, #0xA800, lsl #16
    br      x0

    // milestone: paint a 32-row band at row `x0` with color `w1` on all 3
    // frame slots (0xD5100000/0xD60B0000/0xD7060000). Clobbers x7-x13.
milestone:
    mov     x7, #3
    adr     x8, slot_bases
1:
    ldr     x9, [x8], #8                // slot base
    mov     x10, #6400
    madd    x9, x0, x10, x9             // x9 = base + y * 6400
    mov     x11, #32                    // 32 rows
2:
    mov     x12, x9
    mov     x13, #1600                  // 1600 pixels per row
3:
    str     w1, [x12], #4
    sub     x13, x13, #1
    cbnz    x13, 3b
    add     x9, x9, x10                 // x10 = 6400 (stride), preserved in loop
    sub     x11, x11, #1
    cbnz    x11, 2b
    sub     x7, x7, #1
    cbnz    x7, 1b
    ret

    .balign 8
slot_bases:
    .quad   0xD5100000
    .quad   0xD60B0000
    .quad   0xD7060000

    .balign 2
boot_msg:
    .byte   'S',0,'I',0,'L',0,'V',0,'E',0,'R',0,' ',0,'B',0,'O',0,'O',0,'T',0,13,0,10,0,0,0

    .section .rdata
    .balign 4096
    .globl  kernel_start
kernel_start:
    .incbin "tablet/Image.nopad"
    .globl  kernel_end
kernel_end:

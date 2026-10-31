// efi_stub.S — PE32+/EFI wrapper around the Silver kernel Image.
//
// Loaded via GRUB `chainloader /Image.efi` + `boot` (StartImage). One boot,
// three sensors:
//   - frame-buffer bands (slot 0, 0xD5100000): GREEN entry, WHITE copy
//     verified, ORANGE pre-MMU-off (+EL2 block at y=128 if EL2), CYAN after
//     VBAR_EL1 install, BLUE before the SCTLR write, VIOLET after it,
//     YELLOW after ic iallu, then br 0xA8000000.
//   - exception trap: all 16 vector entries point at one inline handler that
//     paints a DARK-RED band (y=224) and renders ESR_EL1 (y=416) and
//     FAR_EL1 (y=448) as 32 bit-blocks (bit31 leftmost, white=1).
//   - pmsg markers ("0=ENTRY", "0=VBAR", "0=EXC") at +0x400 of both pmsg
//     zones (0xA2040000 Android/DTB, 0xA2200000 module params).
// After VBAR install NO bl/milestone: every paint is inline (traps must be
// catchable with no call stack and no unresolved PC-relative surprises).
//
// Built with clang (COFF) + lld-link (ImageBase=0): ./tablet/build_efi_img.sh

    .text
    .globl  efi_entry

// band: inline paint of a 32-row band on slot 0 (0xD5100000), 1600 px/row.
// Clobbers x12-x17.
.macro band y, color
    movz    x16, #0xD510, lsl #16
    movz    x17, #(\y)
    mov     x14, #6400
    mul     x17, x17, x14         // offset = y * 6400
    add     x16, x16, x17
    mov     x17, #6400              // row stride (x17 reused)
    mov     w15, #(\color)
    mov     w14, #1600
    mov     w13, #32
9991:
    mov     x12, x16
9992:
    str     w15, [x12], #4
    sub     w14, w14, #1
    cbnz    w14, 9992b
    add     x16, x16, x17
    mov     w14, #1600
    sub     w13, w13, #1
    cbnz    w13, 9991b
.endm

// bits: render the low 32 bits of \val as 32 blocks of 8x32 px at row \y,
// bit 31 leftmost, white=1 / black=0. Inline. Clobbers x9-x17.
.macro bits val, y
    movz    x16, #0xD510, lsl #16
    movz    x17, #(\y)
    mov     x14, #6400
    mul     x17, x17, x14         // offset = y * 6400
    add     x16, x16, x17        // x16 = left edge of the bit row
    mov     x17, #6400           // row stride
    mov     x12, \val
    mov     x13, #32             // 32 blocks
9961:
    tst     x12, #(1 << 31)
    mov     w14, #0xFFFFFFFF     // white = 1
    mov     w15, #0x00000000     // black = 0
    csel    w15, w14, w15, ne
    mov     x11, x16
    mov     x10, #32             // rows per block
9962:
    mov     x9, x11
    mov     x14, #8              // pixels per block
9963:
    str     w15, [x9], #4
    sub     x14, x14, #1
    cbnz    x14, 9963b
    add     x11, x11, x17
    sub     x10, x10, #1
    cbnz    x10, 9962b
    add     x16, x16, #32        // next block (8 px * 4 bytes)
    lsl     x12, x12, #1
    sub     x13, x13, #1
    cbnz    x13, 9961b
.endm

// pmark: store 8 bytes (4 halfwords, little-endian ASCII, NUL-padded) at
// +0x400 of both pmsg zones and clean to PoC. Inline. Clobbers x16, x17.
.macro pmark hw0, hw1, hw2, hw3
    movz    x16, #\hw0
    movk    x16, #\hw1, lsl #16
    movk    x16, #\hw2, lsl #32
    movk    x16, #\hw3, lsl #48
    movz    x17, #0xA204, lsl #16
    add     x17, x17, #0x400
    str     x16, [x17]
    dc      cvac, x17
    movz    x17, #0xA220, lsl #16
    add     x17, x17, #0x400
    str     x16, [x17]
    dc      cvac, x17
    dsb     sy
.endm

efi_entry:
    mov     x19, x1                     // save SystemTable (callee-saved)

    // --- GREEN: entry ------------------------------------------------------
    mov     x0, #0
    mov     w1, #0xFF00
    movk    w1, #0xFF00, lsl #16        // 0xFF00FF00 green
    bl      milestone

    pmark   0x3D30, 0x454E, 0x5452, 0x0A59   // pmsg "0=ENTRY\n" (calibration)

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
    mov     x20, x2                     // x20 = dest base (survives milestone)

    // --- 5. Clean the fixed range [0xA8000000, 0xA8000000+KERNEL_SIZE) ----
    .set    KERNEL_SIZE, 0x200000       // 2 MiB, with margin
    movz    x0, #0xA800, lsl #16
    movz    x1, #0xA800, lsl #16
    movz    x3, #(KERNEL_SIZE >> 16), lsl #16
    add     x1, x1, x3
    mrs     x15, ctr_el0
    ubfx    x15, x15, #16, #4
    mov     x6, #4
    lsl     x6, x6, x15
clean_loop:
    dc      cvac, x0
    add     x0, x0, x6
    cmp     x0, x1
    b.lo    clean_loop
    dsb     sy

    // --- 5b. Verify the copy: compare 2 words at 0xA8000000 ----------------
    adrp    x3, kernel_start
    add     x3, x3, :lo12:kernel_start
    ldr     x5, [x3]
    ldr     x6, [x20]
    ldr     x7, [x3, #8]
    ldr     x8, [x20, #8]
    eor     x5, x5, x6
    eor     x7, x7, x8
    orr     x5, x5, x7
    mov     x0, #32
    cbnz    x5, copy_bad
    mov     w1, #0xFFFF
    movk    w1, #0xFFFF, lsl #16        // 0xFFFFFFFF white = copy OK
    bl      milestone
    b       copy_verified

copy_bad:
    mov     w1, #0x0000
    movk    w1, #0xFFFF, lsl #16        // 0xFFFF0000 red = copy mismatch
    bl      milestone

copy_verified:
    // --- 6. ORANGE: right before MMU off -----------------------------------
    mov     x0, #64
    mov     w1, #0x8000
    movk    w1, #0xFFFF, lsl #16        // 0xFFFF8000 orange
    bl      milestone

    // EL indicator: filled ORANGE block at y=128 if EL2, empty if EL1
    mrs     x13, CurrentEL
    lsr     x13, x13, #2
    cmp     x13, #2
    b.ne    el1_mark
    mov     x0, #128
    mov     w1, #0x8000
    movk    w1, #0xFFFF, lsl #16
    bl      milestone
el1_mark:

    // --- 7. Trap installation: VBAR_EL1 -> vbar_table ----------------------
    adr     x14, vbar_table
    msr     vbar_el1, x14
    isb
    band    160, 0xFF00FFFF             // CYAN: VBAR_EL1 written + isb done
    pmark   0x3D30, 0x4256, 0x5241, 0x000A   // pmsg "0=VBAR\n"

    // --- 8. MMU off (EL-conditional), inline everything from here ----------
    band    320, 0xFF0000FF             // BLUE: right before the SCTLR write
    msr     daifset, #0xf               // mask async exceptions first
    movz    x9, #0xA800, lsl #16        // jump target (inline code keeps x9)
    mrs     x10, CurrentEL
    lsr     x10, x10, #2
    cmp     x10, #2
    b.eq    1f
    // EL1
    mrs     x11, sctlr_el1
    bic     x11, x11, #1                // M
    bic     x11, x11, #4                // C
    bic     x11, x11, #(1 << 12)        // I
    msr     sctlr_el1, x11
    b       2f
1:  // EL2
    mrs     x11, sctlr_el2
    bic     x11, x11, #1                // M
    bic     x11, x11, #4                // C
    bic     x11, x11, #(1 << 12)        // I
    msr     sctlr_el2, x11
2:  isb
    band    352, 0xFFFF0080             // VIOLET: SCTLR write + isb survived
    ic      iallu
    dsb     sy
    isb
    band    384, 0xFFFFFF00             // YELLOW: ic iallu survived

    br      x9                          // -> 0xA8000000

    // --- trap handler: all 16 entries land here, inline, no stack ----------
    .balign 0x800
vbar_table:
    .rept 16
    b       handler
    .space  0x80 - 4
    .endr

handler:
    msr     daifset, #0xf
    mrs     x19, esr_el1
    mrs     x20, far_el1
    mrs     x21, elr_el1
    band    224, 0xFF800000             // dark red: exception caught at EL1
    bits    x19, 416                    // ESR_EL1 as bit blocks
    bits    x20, 448                    // FAR_EL1 as bit blocks
    pmark   0x3D30, 0x5845, 0x0A43, 0x0000   // pmsg "0=EXC\n"
9970:
    wfe
    b       9970b

    // milestone: paint a 32-row band at row `x0` with color `w1` on all 3
    // frame slots (0xD5100000/0xD60B0000/0xD7060000). Clobbers x7-x13.
    // Only used BEFORE vbar_table is installed.
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

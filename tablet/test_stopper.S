// Minimal boot-image probe for the SM8650 ABL (Kimi isolation experiment #1).
//
// The payload is 64-byte arm64 Image header + two instructions at 0x40:
// wfe / b .  — it can only freeze the machine, never crash it. If the tablet
// hangs on a black screen after `fastboot boot`, ABL executed our payload and
// the problem is in the real kernel; if it still QCDUMPs, ABL rejects the
// image itself (format/load path), before any of our code runs.

.section .text.boot, "ax"
.global _head
_head:
    b       1f                          // 0x00 code0: jump past header
    .word   0                           // 0x04 code1
    .quad   0xA8000000                  // 0x08 text_offset (linked base)
    .quad   _end - _head                // 0x10 image_size
    .quad   0                           // 0x18 flags
    .quad   0                           // 0x20 reserved
    .quad   0                           // 0x28 reserved
    .quad   0                           // 0x30 reserved
    .word   0x644D5241                  // 0x38 magic "ARM\x64"
    .word   0                           // 0x3C reserved
1:
    wfe                                 // 0x40: park the primary core
    b 1b                                // forever (no event can wake it for long)
.global _end
_end:

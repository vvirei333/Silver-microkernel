// src/main.rs: exception vectors (lines 1048-1092)
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
    // ESR decoding needs scratch registers, but x9/x10/x11 still belong to
    // the user. Preserve all three before the SVC check so optimized
    // userspace can keep live values in any register across svc #0 and so the
    // COW fault frame saves the real x11.

// src/main.rs: IRQ trampoline, signal-return hook, epilogue (1185-1345)
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
    // Preserve the EL0 vector register file: kernel code may use SIMD.
    mrs x10, tpidr_el1
    add x10, x10, {fp_offset}
    mov x0, x10
    bl silver_fp_save
    mov x0, sp
    bl silver_handle_syscall
    // Fall through to the epilogue: the handler may have switched this task
    // away and back (the switch resumes via `ret`, unwinding the call chain
    // to this exact point), so the frame and SP are exactly as we left them.
exception_svc_epilogue:
    // Restore the task's vector registers; must stay the last step before
    // the saved register bank is reloaded and `eret` runs.
    mrs x10, tpidr_el1
    add x10, x10, {fp_offset}
    mov x0, x10
    bl silver_fp_restore
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
    // Only an exception taken from EL0 carries a user vector file.
    mrs x10, spsr_el1
    and x10, x10, #0x0f
    cbnz x10, 8f
    // Preserve the EL0 vector register file: kernel code may use SIMD.
    mrs x10, tpidr_el1
    add x10, x10, {fp_offset}
    mov x0, x10
    bl silver_fp_save
8:
    bl silver_handle_irq
    adrp x9, {probe}
    mov w10, #17
    str w10, [x9, #:lo12:{probe}]
    // Async Linux signals: deliver pending handlers now, while the user
    // register bank is still on this stack and before the epilogue publishes
    // ELR/SPSR. The hook is a no-op for native tasks and EL1 interrupts.
    mov x0, sp
    bl silver_irq_signal_return
    // Fall through to the epilogue (see the SVC trampoline above).
exception_irq_epilogue:
    ldr x10, [sp, #256]
    and x10, x10, #0x0f
    cbnz x10, 8f
    // Restore the task's vector registers; must stay the last step before
    // the saved register bank is reloaded and `eret` runs.
    mrs x10, tpidr_el1
    add x10, x10, {fp_offset}
    mov x0, x10
    bl silver_fp_restore
8:
    ldr x9, [sp, #248]
    msr elr_el1, x9
    ldr x9, [sp, #256]
    msr spsr_el1, x9
    ldr x30, [sp, #240]

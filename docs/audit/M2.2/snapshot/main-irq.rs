// Excerpt from src/main.rs: IRQ vector + trampoline, lines 1240-1375
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

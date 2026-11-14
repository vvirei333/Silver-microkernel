// Excerpt from src/main.rs: the EL0 sync-fault vector, lines 1100-1180
    // Anything else: if it came from EL0 it is a task fault (bad pointer,
    // bad instruction, trapped MSR...). If it came from EL1 it is a kernel
    // bug and the normal exception path panics.
    mrs x9, spsr_el1
    and x9, x9, #0xC         // SPSR.M[3:2]: 0b00 = EL0t
    cbnz x9, exception_sync_kernel
    ldp x9, x10, [sp]
    ldr x11, [sp, #16]
    add sp, sp, #32
    b exception_user_fault
exception_sync_kernel:
    ldp x9, x10, [sp]
    ldr x11, [sp, #16]
    add sp, sp, #32
    mov x0, #0
    b exception_dispatch
exception_sync_svc:
    ldp x9, x10, [sp]
    ldr x11, [sp, #16]
    add sp, sp, #32
    b exception_svc_dispatch
exception_user_fault:
    // Save the full EL0 bank so a COW fault can be repaired and the faulting
    // instruction retried. Non-COW faults still end in silver_handle_task_fault.
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
    mrs x1, esr_el1
    mrs x2, elr_el1
    mrs x3, far_el1
    bl silver_handle_user_fault
    // Recovered: publish ELR/SPSR and return to the retried instruction.
    ldr x9, [sp, #248]
    msr elr_el1, x9
    ldr x9, [sp, #256]
    msr spsr_el1, x9
    // Restore the task's vector registers; must stay the last step before
    // the saved register bank is reloaded and `eret` runs.
    mrs x10, tpidr_el1
    add x10, x10, {fp_offset}
    mov x0, x10
    bl silver_fp_restore
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

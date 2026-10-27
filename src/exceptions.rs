use crate::pl011;
use crate::scheduler;

#[no_mangle]
pub extern "C" fn silver_handle_task_fault(esr: u64, elr: u64, far: u64) -> ! {
    // Reached from the EL0 sync vector with IRQs already masked and x0..x2
    // carrying ESR_EL1/ELR_EL1/FAR_EL1. pl011 writes are raw MMIO pokes whose
    // TX wait restores the caller's DAIF, so they are safe in exception
    // context; nothing here may allocate or take a lock (the dying task may
    // still hold one). Format matches the other kernel dumps: fixed-width
    // uppercase hex, \r\n line ends.
    pl011::write_str("EL0 SYNC FAULT!\r\n");
    pl011::write_str("ESR_EL1=");
    pl011::write_hex_u64(esr);
    pl011::write_str("\r\n");
    pl011::write_str("ELR_EL1=");
    pl011::write_hex_u64(elr);
    pl011::write_str("\r\n");
    pl011::write_str("FAR_EL1=");
    pl011::write_hex_u64(far);
    pl011::write_str("\r\n");
    scheduler::dump_switch_trace();
    // The task touched something it must not (unmapped VA, privileged MSR,
    // EL1-only MMIO...). The kernel survives; the guilty task does not.
    scheduler::exit_current();
}
#[no_mangle]
pub extern "C" fn silver_handle_exception(kind: u64, esr: u64, elr: u64, far: u64) -> ! {
    // EL1 path. Previously silent, which made a fault inside spawn or the user
    // bootstrap indistinguishable from a task that simply finished: the task
    // vanished with no trace. These are the same raw pl011 pokes the EL0 dump
    // uses, so they are safe here (no allocation, no locks).
    pl011::write_str("EL1 EXCEPTION kind=");
    pl011::write_dec_u64(kind);
    pl011::write_str("\r\n");
    pl011::write_str("ESR_EL1=");
    pl011::write_hex_u64(esr);
    pl011::write_str("\r\n");
    pl011::write_str("ELR_EL1=");
    pl011::write_hex_u64(elr);
    pl011::write_str("\r\n");
    pl011::write_str("FAR_EL1=");
    pl011::write_hex_u64(far);
    pl011::write_str("\r\n");
    scheduler::exit_current();
}

use crate::{mmu, pl011, scheduler};

#[no_mangle]
pub extern "C" fn silver_handle_task_fault(esr: u64, elr: u64, far: u64) -> ! {
    pl011::write_str("TASK FAULT\r\nID=");
    pl011::write_dec_u64(scheduler::current_task_id());
    pl011::write_str("\r\nEC=");
    pl011::write_hex_u64(esr >> 26);
    pl011::write_str("\r\nESR=");
    pl011::write_hex_u64(esr);
    pl011::write_str("\r\nELR=");
    pl011::write_hex_u64(elr);
    pl011::write_str("\r\nFAR=");
    pl011::write_hex_u64(far);
    pl011::write_str("\r\nSP_EL0=");
    let sp_el0: u64;
    unsafe {
        core::arch::asm!("mrs {}, sp_el0", out(reg) sp_el0);
    }
    pl011::write_hex_u64(sp_el0);
    pl011::write_str("\r\nSTACK=");
    for index in 0..8_u64 {
        let Some(address) = sp_el0.checked_add(index * 8) else {
            break;
        };
        if !mmu::valid_user_slice(address, 8) {
            pl011::write_str("<unmapped>");
            break;
        }
        let value = unsafe { core::ptr::read_volatile(address as *const u64) };
        pl011::write_hex_u64(value);
        pl011::write_str(" ");
    }
    pl011::write_str("\r\n");
    scheduler::dump_switch_trace();
    // The task touched something it must not (unmapped VA, privileged MSR,
    // EL1-only MMIO...). The kernel survives: the guilty task does not.
    scheduler::exit_current();
}
#[no_mangle]
pub extern "C" fn silver_handle_exception(kind: u64, esr: u64, elr: u64, far: u64) -> ! {
    let kind_name = match kind {
        0 => "SYNC",
        1 => "IRQ",
        2 => "FIQ",
        3 => "SERROR",
        4 => "PANIC",
        5 => "MEMORY",
        6 => "ALLOCATOR",
        7 => "HEAP",
        8 => "INTERRUPT",
        _ => "UNKNOWN",
    };

    pl011::write_str("KERNEL PANIC\r\nKIND=");
    pl011::write_str(kind_name);
    pl011::write_str("\r\nEC=");
    pl011::write_hex_u64(esr >> 26);
    pl011::write_str("\r\nESR=");
    pl011::write_hex_u64(esr);
    pl011::write_str("\r\nELR=");
    pl011::write_hex_u64(elr);
    pl011::write_str("\r\nFAR=");
    pl011::write_hex_u64(far);

    loop {
        unsafe {
            core::arch::asm!("wfe", options(nomem, nostack));
        }
    }
}

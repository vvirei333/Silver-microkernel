use crate::pl011;

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

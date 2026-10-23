pub fn enable_irq() {
    unsafe {
        core::arch::asm!("msr daifclr, #2", options(nomem, nostack));
    }
}

pub fn wait_for_irq() {
    unsafe {
        core::arch::asm!("wfi", options(nomem, nostack));
    }
}

const IRQ_MASK: u64 = 1 << 7;

pub fn enable_irq() {
    unsafe {
        core::arch::asm!("msr daifclr, #2", options(nomem, nostack));
    }
}

pub fn save_and_disable_irq() -> u64 {
    let daif: u64;
    unsafe {
        core::arch::asm!("mrs {}, daif", out(reg) daif, options(nostack));
        core::arch::asm!("msr daifset, #2", options(nostack));
    }
    daif
}

pub fn restore_irq(saved_daif: u64) {
    if saved_daif & IRQ_MASK == 0 {
        enable_irq();
    }
}

pub fn wait_for_irq() {
    unsafe {
        core::arch::asm!("wfi", options(nomem, nostack));
    }
}

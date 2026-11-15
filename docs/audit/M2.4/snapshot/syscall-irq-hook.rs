// src/syscall.rs: silver_irq_signal_return (195-220)
static DEMO_CHANNEL: channel::Channel<u64> = channel::Channel::new(2);

/// IRQ-return hook for asynchronous Linux signals. The assembly trampoline
/// passes the saved user register bank (x0-x30, ELR, SPSR) before the epilogue
/// publishes ELR/SPSR. Only an EL0t context can receive a signal; an
/// interrupt taken in EL1 (kernel code) is left alone.
#[no_mangle]
pub extern "C" fn silver_irq_signal_return(frame: *mut u64) {
    if frame.is_null() {
        return;
    }
    let frame = unsafe { &mut *(frame as *mut [u64; FRAME_QUADWORDS]) };
    // Same gate as the sigreturn sanitizer: only a native AArch64 EL0t
    // context (M[4:0] = 0) may receive a signal frame. M[4] would select
    // AArch32, which has no signal-frame layout here.
    if frame[32] & 0x1F != 0 {
        return;
    }
    #[cfg(feature = "linux-abi")]
    if scheduler::current_personality() == scheduler::Personality::Linux {
        let _kernel_lock = crate::smp::BklGuard::new();
        crate::linux_signal::deliver_pending(frame);
    }
}

/// Entry point for the SVC trampoline. `frame` points at the saved register

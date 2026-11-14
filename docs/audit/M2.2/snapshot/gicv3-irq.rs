// Excerpt from src/gicv3.rs: GICv3 IRQ dispatch, lines 700-780
            value = in(reg) u64::from(acknowledge_register),
            options(nomem, nostack),
        );
    }
}

/// Top-level IRQ path from `exception_vectors` on the tablet. Mirrors the GICv2
/// dispatch in `gic.rs`: the timer reschedules, everything else bound to a task
/// wakes it, and an unknown INTID is masked in its proven scope. Only an
/// unknown INTID that cannot be masked reaches the exception handler, after a
/// bounded number of attempts.
pub fn handle_irq() {
    let acknowledge_register = acknowledge();
    let irq = acknowledge_register & 0x3FF;

    match irq {
        RESCHEDULE_IRQ => {
            end_interrupt(acknowledge_register);
            if crate::smp::irq_may_schedule() {
                scheduler::schedule();
            }
        }
        crate::gic::QUIESCE_IRQ => {
            end_interrupt(acknowledge_register);
            crate::smp::quiesce_ack_and_wait();
        }
        SPURIOUS_IRQ => {}
        _ if irq == timer_irq() => {
            timer::handle_irq();
            end_interrupt(acknowledge_register);
            // Diagnostic builds arm a one-shot PSCI reset; checking it on the
            // tick keeps the deadline independent of scheduler load.
            #[cfg(feature = "fog-auto-reset")]
            crate::fog::auto_reset_poll();
            if crate::smp::irq_may_schedule() {
                scheduler::schedule();
            }
        }
        _ => {
            // Probe and wake are one kernel-lock-protected step (see gic.rs).
            // Splitting them into two windows lets another CPU unbind the
            // INTID in between, after which this path masks the line and
            // wakes nobody: the interrupt is silently lost.
            {
                let _bkl = crate::smp::BklGuard::new();
                if scheduler::irq_is_bound(irq) {
                    disable_irq(irq);
                    end_interrupt(acknowledge_register);
                    scheduler::wake_bound_irq(irq);
                    return;
                }
            }
            // No driver owns this INTID. Mask it before EOI so a level
            // source cannot re-enter immediately. If no proven scope exists,
            // `note_unknown_irq` allows only a bounded streak before the
            // exception path stops the machine.
            let masked = crate::gic::mask_irq(irq);
            end_interrupt(acknowledge_register);
            if !crate::gic::note_unknown_irq(irq, masked) {
                exceptions::silver_handle_exception(8, u64::from(irq), 0, 0);
            }
        }
    }
}

fn read32(address: usize) -> u32 {
    unsafe { ptr::read_volatile(address as *const u32) }
}

fn read64(address: usize) -> u64 {
    unsafe { ptr::read_volatile(address as *const u64) }
}

fn write32(address: usize, value: u32) {
    unsafe { ptr::write_volatile(address as *mut u32, value) }
}

fn write8(address: usize, value: u8) {
    unsafe { ptr::write_volatile(address as *mut u8, value) }
}

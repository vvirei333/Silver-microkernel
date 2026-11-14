// Excerpt from src/gic.rs: GICv2 IRQ dispatch, lines 335-470
    if platform::detect().uses_gicv3() {
        return gicv3::acknowledge();
    }
    read32(CPU_INTERFACE_BASE + GICC_IAR)
}

pub fn end_interrupt(acknowledge_register: u32) {
    if platform::detect().uses_gicv3() {
        gicv3::end_interrupt(acknowledge_register);
        return;
    }
    write32(CPU_INTERFACE_BASE + GICC_EOIR, acknowledge_register);
}

#[no_mangle]
pub extern "C" fn silver_handle_irq() {
    if platform::detect().uses_gicv3() {
        return gicv3::handle_irq();
    }

    crate::smp::probe(19);
    let acknowledge_register = acknowledge();
    let irq = acknowledge_register & 0x3FF;

    if irq != TIMER_IRQ && irq != RESCHEDULE_IRQ && irq != QUIESCE_IRQ && irq != SPURIOUS_IRQ {
        // The probe and the wake must be one kernel-lock-protected step:
        // another CPU could otherwise change the binding in between.
        let _bkl = crate::smp::BklGuard::new();
        if scheduler::irq_is_bound(irq) {
            disable_irq(irq);
            end_interrupt(acknowledge_register);
            scheduler::wake_bound_irq(irq);
            return;
        }
    }

    match irq {
        RESCHEDULE_IRQ => {
            // Another CPU made a task Ready (or otherwise changed scheduler
            // state): re-evaluate our run queue. EOI first so the SGI can be
            // re-raised while we schedule. A task inside a syscall holds the
            // kernel lock and schedules at its own blocking point instead.
            end_interrupt(acknowledge_register);
            if crate::smp::irq_may_schedule() {
                scheduler::schedule();
            }
        }
        QUIESCE_IRQ => {
            end_interrupt(acknowledge_register);
            crate::smp::quiesce_ack_and_wait();
        }
        TIMER_IRQ => {
            crate::smp::probe(20);
            timer::handle_irq();
            end_interrupt(acknowledge_register);
            if crate::smp::irq_may_schedule() {
                scheduler::schedule();
            }
        }
        UART_IRQ => {
            console::handle_irq();
            let stopped = console::take_stop_request() && scheduler::stop_console_owner();
            end_interrupt(acknowledge_register);
            // Ordinary input only fills the RX queue. Ctrl-Z changes scheduler
            // state and must switch back to the shell immediately; a task
            // inside a syscall holds the lock and will reach its own
            // scheduling point.
            if stopped && crate::smp::irq_may_schedule() {
                scheduler::schedule();
            }
        }
        SPURIOUS_IRQ => {}
        _ => {
            // No driver owns this INTID. Mask the specific line in its own
            // scope first, then EOI so the active state is released. If the
            // mask cannot be proven, the bounded fallback below halts
            // instead of accepting an endless storm.
            let masked = mask_irq(irq);
            end_interrupt(acknowledge_register);
            if !note_unknown_irq(irq, masked) {
                exceptions::silver_handle_exception(8, u64::from(irq), 0, 0);
            }
        }
    }
}

fn read32(address: usize) -> u32 {
    unsafe { ptr::read_volatile(address as *const u32) }
}

fn write32(address: usize, value: u32) {
    unsafe { ptr::write_volatile(address as *mut u32, value) }
}

fn write8(address: usize, value: u8) {
    unsafe { ptr::write_volatile(address as *mut u8, value) }
}

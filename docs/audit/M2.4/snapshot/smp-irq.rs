// src/smp.rs: irq_may_schedule (965-985)

/// Kernel-lock depth on this CPU (0 = not held). Used by kernel-task entry
/// code that must drop a lock inherited from its dispatcher.
pub fn bkl_depth() -> u32 {
    this_cpu().bkl_depth.load(Ordering::Relaxed)
}

/// True when an IRQ handler may run a scheduling pass. The interrupted
/// context must not already hold the kernel lock: a pass taken on top of a
/// task that is inside a syscall adds a second hold, and the block of the
/// next task consumes it, breaking the per-CPU depth accounting that carries
/// the lock across a context switch. Kernel code is therefore non-preemptible
/// under the BKL; it schedules at its own blocking points.
pub fn irq_may_schedule() -> bool {
    bkl_depth() == 0
}

/// Non-inline wrapper for asm callers (`silver_user_bootstrap` drops the
/// inherited kernel lock before `eret`-ing into user mode).
pub extern "C" fn bkl_release_asm() {
    bkl_release();

//! Pure IRQ classification for SGI, PPI, SPI and spurious INTIDs.
//!
//! The runtime dispatch lives in `src/gic.rs` (GICv2) and `src/gicv3.rs`
//! (GICv3). This module only owns the rules that decide whether a userspace
//! task may bind an INTID and when an unmaskable unknown interrupt must stop
//! the machine. Keeping them here makes the policy host-testable without
//! touching MMIO or scheduler state.

/// GIC spurious interrupt ID: the acknowledge register reports 1023 when no
/// interrupt is pending for this CPU.
pub const SPURIOUS_INTID: u32 = 1023;

/// First INTID in the shared peripheral interrupt range.
pub const FIRST_SPI: u32 = 32;

/// Last architecturally defined INTID. 1020-1022 are reserved and 1023 is
/// spurious; none of them is a bindable device interrupt.
pub const LAST_SPI: u32 = 1019;

/// Number of consecutive unmaskable unknown interrupts tolerated before the
/// kernel halts. Masking a delivered INTID stops a storm; when the GIC frame
/// needed for that mask cannot be proven, continuing forever is not safe.
pub const UNMASKABLE_HALT_LIMIT: u32 = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IrqClass {
    /// INTID 0-15, banked per PE.
    Sgi,
    /// INTID 16-31, banked per PE.
    Ppi,
    /// INTID 32-1019, routed by the distributor.
    Spi,
    /// INTID 1023.
    Spurious,
    /// INTID 1020-1022 and anything above 1023.
    Reserved,
}

pub fn classify(intid: u32) -> IrqClass {
    match intid {
        0..=15 => IrqClass::Sgi,
        16..=31 => IrqClass::Ppi,
        32..=1019 => IrqClass::Spi,
        1023 => IrqClass::Spurious,
        _ => IrqClass::Reserved,
    }
}

/// Whether `SYS_BIND_IRQ` may hand `intid` to a task holding `CAP_IRQ`.
///
/// Only shared peripheral interrupts are bindable. SGIs and PPIs are banked
/// per PE and include kernel-private reschedule and timer interrupts; a
/// userspace task must never own or acknowledge those. The timer INTID is
/// rejected explicitly even though it is already a PPI, so the rule stays
/// correct if a future platform wires the timer as an SPI.
pub fn is_user_bindable(intid: u32, timer: u32) -> bool {
    classify(intid) == IrqClass::Spi && intid != timer
}

/// Register scope that owns the enable bit for an INTID.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MaskScope {
    /// SGI/PPI: banked per PE in the redistributor (GICv3 `GICR_ICENABLER0`)
    /// or in the banked distributor word 0 (GICv2).
    Private,
    /// SPI: the shared `GICD_ICENABLER<n>` word in the distributor.
    Shared,
    /// Spurious/reserved: there is no valid mask register.
    Unavailable,
}

pub fn mask_scope(intid: u32) -> MaskScope {
    match classify(intid) {
        IrqClass::Sgi | IrqClass::Ppi => MaskScope::Private,
        IrqClass::Spi => MaskScope::Shared,
        IrqClass::Spurious | IrqClass::Reserved => MaskScope::Unavailable,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnmaskableAction {
    Continue,
    Halt,
}

/// Decides what to do after an unknown INTID could not be masked in a proven
/// scope. The streak is reset by every successful mask.
pub fn unmaskable_streak_action(streak: u32) -> UnmaskableAction {
    if streak >= UNMASKABLE_HALT_LIMIT {
        UnmaskableAction::Halt
    } else {
        UnmaskableAction::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_every_architectural_range() {
        assert_eq!(classify(0), IrqClass::Sgi);
        assert_eq!(classify(15), IrqClass::Sgi);
        assert_eq!(classify(16), IrqClass::Ppi);
        assert_eq!(classify(31), IrqClass::Ppi);
        assert_eq!(classify(32), IrqClass::Spi);
        assert_eq!(classify(1019), IrqClass::Spi);
        assert_eq!(classify(1020), IrqClass::Reserved);
        assert_eq!(classify(1022), IrqClass::Reserved);
        assert_eq!(classify(1023), IrqClass::Spurious);
        assert_eq!(classify(1024), IrqClass::Reserved);
        assert_eq!(classify(u32::MAX), IrqClass::Reserved);
    }

    #[test]
    fn mask_scope_selects_the_register_that_owns_the_intid() {
        // SGIs and PPIs are banked per PE; SPIs are distributor-wide.
        assert_eq!(mask_scope(0), MaskScope::Private);
        assert_eq!(mask_scope(15), MaskScope::Private);
        assert_eq!(mask_scope(16), MaskScope::Private);
        assert_eq!(mask_scope(31), MaskScope::Private);
        assert_eq!(mask_scope(32), MaskScope::Shared);
        assert_eq!(mask_scope(1019), MaskScope::Shared);
        // No mask register exists for spurious or reserved INTIDs.
        assert_eq!(mask_scope(1020), MaskScope::Unavailable);
        assert_eq!(mask_scope(1023), MaskScope::Unavailable);
        assert_eq!(mask_scope(u32::MAX), MaskScope::Unavailable);
    }

    #[test]
    fn only_spis_are_user_bindable() {
        // Kernel-private SGI/PPI, including reschedule SGI 0 and both timer
        // PPIs seen on QEMU and khaje, must be rejected.
        for intid in [0, 1, 15, 16, 17, 18, 19, 30, 31] {
            assert!(!is_user_bindable(intid, 18), "INTID {intid} must not bind");
            assert!(!is_user_bindable(intid, 30), "INTID {intid} must not bind");
        }
        // Spurious and reserved IDs are never bindable.
        for intid in [1020, 1021, 1022, 1023, 1024, u32::MAX] {
            assert!(!is_user_bindable(intid, 18), "INTID {intid} must not bind");
        }
        // A normal SPI is bindable; the timer exclusion stays effective if a
        // future platform reports it as an SPI.
        assert!(is_user_bindable(32, 18));
        assert!(is_user_bindable(33, 18));
        assert!(is_user_bindable(400, 18));
        assert!(is_user_bindable(1019, 18));
        assert!(!is_user_bindable(400, 400));
    }

    #[test]
    fn unmaskable_unknown_irq_has_a_finite_limit() {
        for streak in 1..UNMASKABLE_HALT_LIMIT {
            assert_eq!(
                unmaskable_streak_action(streak),
                UnmaskableAction::Continue
            );
        }
        assert_eq!(
            unmaskable_streak_action(UNMASKABLE_HALT_LIMIT),
            UnmaskableAction::Halt
        );
        assert_eq!(
            unmaskable_streak_action(UNMASKABLE_HALT_LIMIT + 1),
            UnmaskableAction::Halt
        );
        assert_eq!(
            unmaskable_streak_action(u32::MAX),
            UnmaskableAction::Halt
        );
    }
}

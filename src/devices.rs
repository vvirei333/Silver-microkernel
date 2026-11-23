//! Device policy: which driver serves a role, given what the machine describes.
//!
//! The point of this module is that the answer depends on the *device tree*,
//! not on the board's name. A machine that describes an `arm,pl011` console
//! gets the PL011 driver, wherever it is; a machine that describes nothing
//! falls back to the identity's built-in map, and a machine with neither is
//! left without a console (silence) instead of being driven at a guessed
//! address.
//!
//! Pure logic only — no MMIO, no allocator — so it is testable on the host.
//! `src/fdt.rs` re-exports [`DriverKind`] and `src/console.rs` applies
//! [`choose_console`].

/// A driver the kernel can bind to a described node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DriverKind {
    Pl011,
    GeniUart,
}

/// How the kernel will talk to the operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConsoleTransport {
    Pl011,
    Geni,
    /// No console this kernel can drive. Every write becomes a no-op.
    None,
}

/// Chooses the console transport.
///
/// * `described` — the UART kind the live device tree named, if any.
/// * `builtin` — the kind the platform's built-in map offers when no tree was
///   passed (QEMU `virt`, the parked fog/tablet profiles), if any.
///
/// The tree wins because it is evidence from the machine itself; the built-in
/// map is a fallback for loaders that pass no tree, never an override.
pub fn choose_console(
    described: Option<DriverKind>,
    builtin: Option<DriverKind>,
) -> ConsoleTransport {
    match described.or(builtin) {
        Some(DriverKind::Pl011) => ConsoleTransport::Pl011,
        Some(DriverKind::GeniUart) => ConsoleTransport::Geni,
        None => ConsoleTransport::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tree_wins_over_the_builtin_map() {
        // A board whose tree says GENI must not be driven by a built-in PL011
        // default (and the reverse): the tree is evidence, the table is a guess.
        assert_eq!(
            choose_console(Some(DriverKind::GeniUart), Some(DriverKind::Pl011)),
            ConsoleTransport::Geni
        );
        assert_eq!(
            choose_console(Some(DriverKind::Pl011), Some(DriverKind::GeniUart)),
            ConsoleTransport::Pl011
        );
    }

    #[test]
    fn a_tree_alone_is_enough() {
        // This is the portable path: a newly described machine works without a
        // platform identity the kernel knows.
        assert_eq!(
            choose_console(Some(DriverKind::Pl011), None),
            ConsoleTransport::Pl011
        );
        assert_eq!(
            choose_console(Some(DriverKind::GeniUart), None),
            ConsoleTransport::Geni
        );
    }

    #[test]
    fn the_builtin_map_is_only_a_fallback() {
        assert_eq!(
            choose_console(None, Some(DriverKind::Pl011)),
            ConsoleTransport::Pl011
        );
    }

    #[test]
    fn nothing_described_means_no_console() {
        // Fails closed: a board with neither a tree nor a built-in map must be
        // silent, never driven at an address copied from another machine.
        assert_eq!(choose_console(None, None), ConsoleTransport::None);
    }
}

//! Pure PCI/PCIe host discovery and configuration-space enumeration.
//!
//! This module deliberately contains no MMIO access: the kernel supplies a
//! [`ConfigSpace`] implementation, so the walk can be exercised on the host
//! with a fake device table. A host is described by the device tree, never by
//! a board name.

/// How a host bridge exposes configuration space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PciHostKind {
    /// Memory-mapped ECAM (`pci-host-ecam-generic`).
    Ecam,
    /// Broadcom BCM2712 DesignWare bridge: index/data registers, not ECAM.
    Bcm2712,
}

/// One host bridge window from the live device tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PciHost {
    pub kind: PciHostKind,
    /// Controller registers for `Bcm2712`, ECAM base for `Ecam`.
    pub base: u64,
    /// Size of the register or ECAM window.
    pub size: u64,
    pub bus_start: u8,
    pub bus_end: u8,
}

impl PciHost {
    pub const fn new(
        kind: PciHostKind,
        base: u64,
        size: u64,
        bus_start: u8,
        bus_end: u8,
    ) -> Self {
        Self {
            kind,
            base,
            size,
            bus_start,
            bus_end,
        }
    }

    pub const fn contains_bus(&self, bus: u8) -> bool {
        bus >= self.bus_start && bus <= self.bus_end
    }
}

/// A function found while walking configuration space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PciDevice {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub vendor: u16,
    pub device_id: u16,
    /// Class code / subclass / programming interface, as returned at 0x08.
    pub class: u32,
    pub header: u8,
}

impl PciDevice {
    /// Raspberry Pi Ltd RP1 south bridge (`1de4:0001`).
    pub const fn is_rp1(&self) -> bool {
        self.vendor == 0x1de4 && self.device_id == 0x0001
    }
}

impl Default for PciDevice {
    fn default() -> Self {
        Self {
            bus: 0,
            device: 0,
            function: 0,
            vendor: 0xffff,
            device_id: 0xffff,
            class: 0,
            header: 0,
        }
    }
}

/// The one hardware operation the walk needs.
pub trait ConfigSpace {
    /// Read 32 bits from `offset` (masked to the 4 KiB config window).
    fn read32(&mut self, bus: u8, device: u8, function: u8, offset: usize) -> u32;
}

/// Walks every function in the host's bus range and records real devices.
///
/// A function is real only when its vendor ID is neither `0xffff` nor zero.
/// Single-function devices are not probed for functions 1..7. The returned
/// count is clamped to `out.len()`; an empty slice is a valid probe that only
/// counts devices.
pub fn scan<C: ConfigSpace>(config: &mut C, host: PciHost, out: &mut [PciDevice]) -> usize {
    if host.bus_start > host.bus_end {
        return 0;
    }
    let mut count = 0usize;
    for bus in host.bus_start..=host.bus_end {
        for device in 0..32u8 {
            let id = config.read32(bus, device, 0, 0x00);
            let vendor = id as u16;
            if vendor == 0xffff || vendor == 0 {
                continue;
            }
            let header = (config.read32(bus, device, 0, 0x0c) >> 16) as u8;
            let functions = if header & 0x80 != 0 { 8 } else { 1 };
            for function in 0..functions {
                let id = if function == 0 {
                    id
                } else {
                    config.read32(bus, device, function, 0x00)
                };
                let vendor = id as u16;
                if vendor == 0xffff || vendor == 0 {
                    continue;
                }
                if count == out.len() {
                    return count;
                }
                out[count] = PciDevice {
                    bus,
                    device,
                    function,
                    vendor,
                    device_id: (id >> 16) as u16,
                    class: config.read32(bus, device, function, 0x08),
                    header,
                };
                count += 1;
            }
        }
    }
    count
}

/// True for an SD host controller (base class 0x08, subclass 0x05).
/// The programming interface is not part of the match: standard SDHCI is
/// 0x01, but some firmware reports 0x00.
pub fn is_sdhci(class: u32) -> bool {
    class & 0xffff_0000 == 0x0805_0000
}

/// Decodes a memory BAR into a 32-bit address the low Device map can reach.
///
/// I/O BARs, reserved BAR types and 64-bit BARs above 4 GiB return `None`.
/// A zero address is returned as zero; the caller decides whether to assign
/// a firmware-free test window.
pub fn memory_bar_address(low: u32, high: u32) -> Option<u32> {
    if low & 1 != 0 {
        return None;
    }
    match (low >> 1) & 0x3 {
        0 => Some(low & !0xf),
        1 => None,
        2 if high == 0 => Some(low & !0xf),
        _ => None,
    }
}

/// First RP1 in a device list, if any.
pub fn find_rp1(devices: &[PciDevice]) -> Option<&PciDevice> {
    devices.iter().find(|device| device.is_rp1())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeFunction {
        bus: u8,
        device: u8,
        function: u8,
        vendor: u16,
        device_id: u16,
        class: u32,
        header: u8,
    }

    struct Fake {
        functions: Vec<FakeFunction>,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                functions: Vec::new(),
            }
        }

        fn add(
            &mut self,
            bus: u8,
            device: u8,
            function: u8,
            vendor: u16,
            device_id: u16,
            class: u32,
            header: u8,
        ) {
            self.functions.push(FakeFunction {
                bus,
                device,
                function,
                vendor,
                device_id,
                class,
                header,
            });
        }
    }

    impl ConfigSpace for Fake {
        fn read32(&mut self, bus: u8, device: u8, function: u8, offset: usize) -> u32 {
            let Some(entry) = self
                .functions
                .iter()
                .find(|entry| {
                    entry.bus == bus && entry.device == device && entry.function == function
                })
            else {
                return 0xffff_ffff;
            };
            match offset {
                0x00 => u32::from(entry.vendor) | (u32::from(entry.device_id) << 16),
                0x08 => entry.class,
                0x0c => u32::from(entry.header) << 16,
                _ => 0,
            }
        }
    }

    fn host() -> PciHost {
        PciHost::new(PciHostKind::Bcm2712, 0x10_0012_0000, 0x9310, 1, 2)
    }

    #[test]
    fn scan_finds_the_rp1_and_keeps_its_ids() {
        let mut fake = Fake::new();
        fake.add(1, 0, 0, 0x1de4, 0x0001, 0x0200_0000, 0x00);

        let mut out = [PciDevice::default(); 4];
        let count = scan(&mut fake, host(), &mut out);

        assert_eq!(count, 1);
        assert_eq!(out[0].bus, 1);
        assert_eq!(out[0].device, 0);
        assert_eq!(out[0].vendor, 0x1de4);
        assert_eq!(out[0].device_id, 0x0001);
        assert_eq!(out[0].class, 0x0200_0000);
        assert!(out[0].is_rp1());
        assert_eq!(find_rp1(&out[..count]).map(|d| d.bus), Some(1));
    }

    #[test]
    fn scan_honours_the_host_bus_range() {
        let mut fake = Fake::new();
        fake.add(0, 0, 0, 0x14e4, 0x2712, 0x0604_0000, 0x00);
        fake.add(1, 0, 0, 0x1de4, 0x0001, 0x0200_0000, 0x00);
        fake.add(3, 0, 0, 0x1de4, 0x0001, 0x0200_0000, 0x00);

        let mut out = [PciDevice::default(); 4];
        let count = scan(&mut fake, host(), &mut out);

        assert_eq!(count, 1);
        assert_eq!(out[0].bus, 1);
    }

    #[test]
    fn scan_walks_every_function_of_a_multifunction_device() {
        let mut fake = Fake::new();
        fake.add(1, 0, 0, 0x1234, 0x0001, 0x0000_0000, 0x80);
        fake.add(1, 0, 1, 0x1234, 0x0002, 0x0000_0000, 0x80);

        let mut out = [PciDevice::default(); 4];
        let count = scan(&mut fake, host(), &mut out);

        assert_eq!(count, 2);
        assert_eq!(out[0].function, 0);
        assert_eq!(out[1].function, 1);
    }

    #[test]
    fn an_empty_slice_counts_but_does_not_store() {
        let mut fake = Fake::new();
        fake.add(1, 0, 0, 0x1de4, 0x0001, 0x0200_0000, 0x00);

        let mut out = [];
        assert_eq!(scan(&mut fake, host(), &mut out), 0);
    }

    #[test]
    fn sdhci_matches_base_class_and_subclass() {
        assert!(is_sdhci(0x0805_0100));
        assert!(is_sdhci(0x0805_0000));
        assert!(!is_sdhci(0x0804_0000));
        assert!(!is_sdhci(0x0200_0000));
    }

    #[test]
    fn memory_bar_decodes_32_and_64_bit_windows() {
        assert_eq!(memory_bar_address(0x1030_0000, 0), Some(0x1030_0000));
        assert_eq!(memory_bar_address(0x1030_0001, 0), None);
        assert_eq!(memory_bar_address(0x0000_0002, 0), None);
        assert_eq!(memory_bar_address(0, 0), Some(0));
        assert_eq!(memory_bar_address(0x1030_0004, 0), Some(0x1030_0000));
        assert_eq!(memory_bar_address(0x1030_0002, 0), None);
        assert_eq!(memory_bar_address(0x1030_0004, 1), None);
    }
}

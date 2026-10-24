#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct MemoryRegion {
    pub start: usize,
    pub end: usize,
}

impl MemoryRegion {
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    pub const fn size(self) -> usize {
        self.end - self.start
    }

    pub const fn is_empty(self) -> bool {
        self.start >= self.end
    }

    pub const fn contains(self, address: usize) -> bool {
        address >= self.start && address < self.end
    }
}

pub const PAGE_SIZE: usize = 4096;
pub const DRAM_BASE: usize = 0x4000_0000;
pub const DRAM_SIZE: usize = 128 * 1024 * 1024;
pub const DRAM_END: usize = DRAM_BASE + DRAM_SIZE;
pub const DRAM: MemoryRegion = MemoryRegion::new(DRAM_BASE, DRAM_END);

/// Link-time base of the kernel's high-half alias. Physical address `p` is
/// available to EL1 at `KERNEL_VIRT_BASE + p` through TTBR1_EL1.
pub const KERNEL_VIRT_BASE: usize = 0xFFFF_0000_0000_0000;

pub const fn phys_to_virt(physical: usize) -> usize {
    KERNEL_VIRT_BASE + physical
}

pub const fn virt_to_phys(virtual_address: usize) -> usize {
    virtual_address - KERNEL_VIRT_BASE
}

extern "C" {
    static __kernel_start: u8;
    static __kernel_end: u8;
    static __stack_bottom: u8;
    static __stack_top: u8;
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct MemoryMap {
    pub dram: MemoryRegion,
    pub kernel_image: MemoryRegion,
    pub kernel_stack: MemoryRegion,
    pub available: MemoryRegion,
}

pub fn current() -> MemoryMap {
    let (kernel_start, kernel_end, stack_bottom, stack_top) = unsafe {
        (
            virt_to_phys(&__kernel_start as *const u8 as usize),
            virt_to_phys(&__kernel_end as *const u8 as usize),
            virt_to_phys(&__stack_bottom as *const u8 as usize),
            virt_to_phys(&__stack_top as *const u8 as usize),
        )
    };

    let kernel_image = MemoryRegion::new(kernel_start, kernel_end);
    let kernel_stack = MemoryRegion::new(stack_bottom, stack_top);

    MemoryMap {
        dram: DRAM,
        kernel_image,
        kernel_stack,
        available: MemoryRegion::new(kernel_image.end, DRAM.end),
    }
}

impl MemoryMap {
    pub fn is_valid(&self) -> bool {
        !self.kernel_image.is_empty()
            && !self.kernel_stack.is_empty()
            && self.dram.contains(self.kernel_image.start)
            && self.kernel_image.end <= self.dram.end
            && self.kernel_stack.start >= self.kernel_image.start
            && self.kernel_stack.end <= self.kernel_image.end
            && self.available.start >= self.kernel_image.end
            && self.available.end == self.dram.end
            && self.available.start % PAGE_SIZE == 0
            && self.available.size() <= self.dram.size()
    }
}

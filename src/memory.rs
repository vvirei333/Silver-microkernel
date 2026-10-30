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
#[cfg(not(feature = "tablet"))]
pub const DRAM_BASE: usize = 0x4000_0000;
#[cfg(not(feature = "tablet"))]
pub const DRAM_SIZE: usize = 128 * 1024 * 1024;
/// Tablet (SM8650): ABL loads the kernel at 0xA800_0000 inside a fragmented
/// 12 GiB LPDDR5X map. The first DDR region is 2 GiB starting at 0x8000_0000,
/// so that is what the MMU covers and what DRAM spans here.
///
/// CAUTION: the chunk 0xA240_0000..0xD4E2_3000 is also System RAM but holds
/// the loaded kernel image and ABL's own structures — nothing may hand those
/// frames out. The frame allocator therefore uses the first clean System RAM
/// chunk after it (0xD510_0000..0xD800_0000), never `kernel_image.end..DRAM.end`.
#[cfg(feature = "tablet")]
pub const DRAM_BASE: usize = 0x8000_0000;
#[cfg(feature = "tablet")]
pub const DRAM_SIZE: usize = 0x8000_0000;
/// First System RAM chunk after the forbidden 0xA240_0000..0xD4E2_3000 region.
/// Its end 0xD800_0000 is the end of the contiguous chunk (`d5100000-d7ffffff`
/// in /proc/iomem), not of DRAM.
/// The first 16 MiB after D510_0000 is the cont_splash framebuffer (panel
/// scanout, 1600x2560x4); frames start above it so the kernel never paints
/// over the screen and the boot progress bars stay visible.
#[cfg(feature = "tablet")]
pub const FRAME_ALLOC_START: usize = 0xD620_0000;
#[cfg(feature = "tablet")]
pub const FRAME_ALLOC_END: usize = 0xD800_0000;
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

    // QEMU: DRAM is one contiguous bank, so everything after the image is free.
    // Tablet: the space after the image is the forbidden ABL/kernel chunk; the
    // allocator takes the clean System RAM chunk after it instead.
    #[cfg(feature = "tablet")]
    let available = MemoryRegion::new(FRAME_ALLOC_START, FRAME_ALLOC_END);
    #[cfg(not(feature = "tablet"))]
    let available = MemoryRegion::new(kernel_image.end, DRAM.end);
    MemoryMap {
        dram: DRAM,
        kernel_image,
        kernel_stack,
        available,
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
            && self.available.end <= self.dram.end
            && self.dram.contains(self.available.start)
            && self.available.start % PAGE_SIZE == 0
            && self.available.size() <= self.dram.size()
    }
}

use core::alloc::{GlobalAlloc, Layout};

use linked_list_allocator::LockedHeap;

use crate::{exceptions, frame_allocator, memory, console};

/// Kernel heap size in 4 KiB pages.
///
/// The console product reserves 4096 pages (16 MiB): enough for the 8 MiB VFS
/// whole-file cap and the 512 KiB RamFs growth test, and 16 MiB less RAM pinned
/// than the Linux-capable build. The Linux ABI keeps 8192 pages (32 MiB) because
/// the ELF loader stages a whole `ld.so`/libc image on the heap and Mesa/GLib
/// clients allocate more temporary state; shrinking them to 16 MiB was not
/// verified, so the feature gate keeps that risk out of the console build.
#[cfg(feature = "linux-abi")]
pub const HEAP_PAGES: usize = 8192;
#[cfg(not(feature = "linux-abi"))]
pub const HEAP_PAGES: usize = 4096;
pub const HEAP_SIZE: usize = HEAP_PAGES * crate::memory::PAGE_SIZE;

struct KernelAllocator {
    inner: LockedHeap,
}

unsafe impl GlobalAlloc for KernelAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { self.inner.alloc(layout) };
        if pointer.is_null() {
            allocation_error(layout);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { self.inner.dealloc(pointer, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: KernelAllocator = KernelAllocator {
    inner: LockedHeap::empty(),
};

/// Returns (used, free, total) bytes of the kernel heap.
///
/// Used is the sum of live allocations; allocator metadata is included in
/// neither figure. The heap lock is held only for the sample, so the caller
/// may use this for health reporting and allocation-failure diagnostics.
pub fn heap_stats() -> (usize, usize, usize) {
    let heap = ALLOCATOR.inner.lock();
    (heap.used(), heap.free(), HEAP_SIZE)
}

/// Allocates a contiguous 1 MiB region and initializes the global kernel heap.
///
/// Returns the physical heap base address on success.
pub fn init() -> Option<usize> {
    let heap_base = frame_allocator::alloc_contiguous_pages(HEAP_PAGES)?;

    unsafe {
        ALLOCATOR
            .inner
            .lock()
            .init(memory::phys_to_virt(heap_base) as *mut u8, HEAP_SIZE);
    }

    Some(heap_base)
}

// `#[alloc_error_handler]` is unstable on Rust 1.99, so GlobalAlloc intercepts
// a failed allocation and reports it here before entering the kernel panic path.
fn allocation_error(layout: Layout) -> ! {
    let (used, free, total) = heap_stats();
    console::write_str("ALLOCATION ERROR\r\nSIZE=");
    console::write_hex_u64(layout.size() as u64);
    console::write_str("\r\nALIGN=");
    console::write_hex_u64(layout.align() as u64);
    console::write_str("\r\nHEAP USED=");
    console::write_hex_u64(used as u64);
    console::write_str(" FREE=");
    console::write_hex_u64(free as u64);
    console::write_str(" TOTAL=");
    console::write_hex_u64(total as u64);
    exceptions::silver_handle_exception(7, 0, 0, 0)
}

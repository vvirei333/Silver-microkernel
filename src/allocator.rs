use core::alloc::{GlobalAlloc, Layout};

use linked_list_allocator::LockedHeap;

use crate::{exceptions, frame_allocator, memory, console};

pub const HEAP_PAGES: usize = 256;
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
    console::write_str("ALLOCATION ERROR\r\nSIZE=");
    console::write_hex_u64(layout.size() as u64);
    console::write_str("\r\nALIGN=");
    console::write_hex_u64(layout.align() as u64);
    exceptions::silver_handle_exception(7, 0, 0, 0)
}

use core::cell::UnsafeCell;

use crate::memory::{MemoryRegion, PAGE_SIZE};

pub const MAX_PHYSICAL_PAGES: usize = crate::memory::DRAM_SIZE / PAGE_SIZE;
const BITS_PER_WORD: usize = 64;
const BITMAP_WORDS: usize = MAX_PHYSICAL_PAGES / BITS_PER_WORD;
const ALL_PAGES_USED: u64 = u64::MAX;

struct FrameAllocatorState {
    bitmap: [u64; BITMAP_WORDS],
    available: MemoryRegion,
    total_pages: usize,
    free_pages: usize,
}

struct SharedState(UnsafeCell<FrameAllocatorState>);

// The kernel is currently single-core and does not enable interrupts before tests finish.
// Add a lock here when interrupt handlers or additional cores access the allocator.
unsafe impl Sync for SharedState {}

static STATE: SharedState = SharedState(UnsafeCell::new(FrameAllocatorState {
    bitmap: [0; BITMAP_WORDS],
    available: MemoryRegion::new(0, 0),
    total_pages: 0,
    free_pages: 0,
}));

fn state() -> &'static mut FrameAllocatorState {
    unsafe { &mut *STATE.0.get() }
}

/// Initializes the allocator from page-aligned part of the available memory region.
///
/// Returns the number of usable physical pages. The bitmap itself lives in the
/// kernel's `.bss`, so it does not consume frames from `available`.
pub fn init(available: MemoryRegion) -> usize {
    let start = align_up(available.start, PAGE_SIZE);
    let end = align_down(available.end, PAGE_SIZE);
    assert!(end > start, "Available memory region is empty: start >= end");
    let requested_pages = if end > start {
        (end - start) / PAGE_SIZE
    } else {
        0
    };
    let total_pages = requested_pages.min(MAX_PHYSICAL_PAGES);
    let usable_end = start + total_pages * PAGE_SIZE;

    let state = state();
    state.bitmap.fill(ALL_PAGES_USED);
    state.available = MemoryRegion::new(start, usable_end);
    state.total_pages = total_pages;
    state.free_pages = total_pages;

    for page_index in 0..total_pages {
        mark_free(state, page_index);
    }

    total_pages
}

pub fn total_pages() -> usize {
    state().total_pages
}

pub fn free_pages() -> usize {
    state().free_pages
}

pub fn used_pages() -> usize {
    let state = state();
    state.total_pages - state.free_pages
}

/// Allocates one physical page using first-fit search.
pub fn alloc_page() -> Option<usize> {
    let state = state();
    let word_count = state.total_pages.div_ceil(BITS_PER_WORD);

    for word_index in 0..word_count {
        let free_mask = !state.bitmap[word_index];
        if free_mask == 0 {
            continue;
        }

        let bit_index = free_mask.trailing_zeros() as usize;
        let page_index = word_index * BITS_PER_WORD + bit_index;
        if page_index >= state.total_pages {
            return None;
        }

        mark_used(state, page_index);
        state.free_pages -= 1;
        return Some(state.available.start + page_index * PAGE_SIZE);
    }

    None
}

/// Allocates a run of `page_count` physically contiguous pages.
///
/// Searches the bitmap for the lowest run of `page_count` consecutive free bits
/// and claims the whole run at once. Fragmented memory is never returned: DMA
/// consumers (VirtIO rings) need one physically contiguous buffer, so a missing
/// run is reported as `None` instead of being spread across the bitmap.
///
/// The state is borrowed exactly once per call. The helpers used below take a
/// `&mut FrameAllocatorState` instead of going back through `alloc_page` or
/// `free_page`, which would borrow the same state a second time.
pub fn alloc_contiguous_pages(page_count: usize) -> Option<usize> {
    if page_count == 0 || page_count > MAX_PHYSICAL_PAGES {
        return None;
    }

    let state = state();
    if page_count > state.free_pages {
        return None;
    }

    let mut run_start = 0_usize;
    let mut run_length = 0_usize;

    for page_index in 0..state.total_pages {
        let word = state.bitmap[page_index / BITS_PER_WORD];
        let is_free = word & (1_u64 << (page_index % BITS_PER_WORD)) == 0;

        if !is_free {
            run_length = 0;
            continue;
        }

        if run_length == 0 {
            run_start = page_index;
        }
        run_length += 1;

        if run_length == page_count {
            for index in run_start..run_start + page_count {
                mark_used(state, index);
            }
            state.free_pages -= page_count;
            return Some(state.available.start + run_start * PAGE_SIZE);
        }
    }

    None
}

/// Returns a previously allocated page to the allocator.
///
/// Invalid and unaligned addresses are ignored; double-free is checked in debug builds.
pub fn free_page(address: usize) {
    let state = state();
    if address % PAGE_SIZE != 0 || !state.available.contains(address) {
        return;
    }

    let page_index = (address - state.available.start) / PAGE_SIZE;
    if page_index >= state.total_pages {
        return;
    }

    let word_index = page_index / BITS_PER_WORD;
    let bit_index = page_index % BITS_PER_WORD;
    let bit_mask = 1_u64 << bit_index;
    let was_used = state.bitmap[word_index] & bit_mask != 0;
    debug_assert!(was_used, "double free of a physical page");

    if was_used {
        state.bitmap[word_index] &= !bit_mask;
        state.free_pages += 1;
    }
}

/// Verifies allocation, contiguity and reuse without changing the final state.
pub fn run_self_test() -> bool {
    let Some(first) = alloc_page() else {
        return false;
    };
    let Some(second) = alloc_page() else {
        free_page(first);
        return false;
    };

    let sequential = second == first + PAGE_SIZE;

    free_page(first);
    let Some(reused) = alloc_page() else {
        free_page(second);
        return false;
    };
    let reuse_ok = reused == first;

    if reused != second {
        free_page(second);
    }
    free_page(reused);

    sequential && reuse_ok && run_contiguous_self_test()
}

/// Allocates the same short run twice. The first run is released before the
/// second attempt, so the bitmap is back to its previous contents and the scan
/// has to return the identical base address.
fn run_contiguous_self_test() -> bool {
    const PROBE_PAGES: usize = 3;

    let Some(first) = alloc_contiguous_pages(PROBE_PAGES) else {
        return false;
    };
    for index in 0..PROBE_PAGES {
        free_page(first + index * PAGE_SIZE);
    }

    let Some(second) = alloc_contiguous_pages(PROBE_PAGES) else {
        return false;
    };
    for index in 0..PROBE_PAGES {
        free_page(second + index * PAGE_SIZE);
    }

    first % PAGE_SIZE == 0 && first == second
}

const fn align_up(address: usize, alignment: usize) -> usize {
    (address + alignment - 1) & !(alignment - 1)
}

const fn align_down(address: usize, alignment: usize) -> usize {
    address & !(alignment - 1)
}

fn mark_used(state: &mut FrameAllocatorState, page_index: usize) {
    let word_index = page_index / BITS_PER_WORD;
    let bit_index = page_index % BITS_PER_WORD;
    state.bitmap[word_index] |= 1_u64 << bit_index;
}

fn mark_free(state: &mut FrameAllocatorState, page_index: usize) {
    let word_index = page_index / BITS_PER_WORD;
    let bit_index = page_index % BITS_PER_WORD;
    state.bitmap[word_index] &= !(1_u64 << bit_index);
}

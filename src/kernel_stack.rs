//! Guarded kernel stacks.
//!
//! Task kernel stacks used to be plain 4-page frame-allocator runs mapped by
//! the kernel's 2 MiB TTBR1 blocks. A downward overflow silently overwrote
//! whichever frame happened to be below the stack. This module reserves one
//! 2 MiB-aligned block, splits its TTBR1 descriptor into 4 KiB pages, and
//! hands out slots laid out as:
//!
//! ```text
//! [ guard ][ stack page 0 ][ stack page 1 ][ stack page 2 ][ stack page 3 ]
//!   4 KiB        4 KiB            4 KiB            4 KiB            4 KiB
//! ```
//!
//! The guard page is never mapped. Stack pages are mapped only while the slot
//! is allocated; freeing a stack unmaps its pages, so even a stale kernel
//! stack pointer faults instead of touching a recycled frame.
//!
//! The arena is permanent after init: its physical pages are not returned to
//! the frame allocator. That is deliberate — the block descriptor is replaced
//! once and remains a page table for the lifetime of the kernel.

use crate::frame_allocator;
use crate::memory::PAGE_SIZE;
use crate::mmu;
use crate::smp::SpinLock;

/// Four stack pages keep the existing 16 KiB task stack size.
pub const STACK_PAGES: usize = 4;
/// One unmapped page below each stack.
const GUARD_PAGES: usize = 1;
const SLOT_PAGES: usize = STACK_PAGES + GUARD_PAGES;
/// 512 pages = 2 MiB; 102 slots fit with two pages left over.
const ARENA_PAGES: usize = 512;
const SLOT_LIMIT: usize = ARENA_PAGES / SLOT_PAGES;

struct Arena {
    base: usize,
    table: usize,
    used: u128,
    initialized: bool,
}

static ARENA: SpinLock<Arena> = SpinLock::new(Arena {
    base: 0,
    table: 0,
    used: 0,
    initialized: false,
});

/// Reserves and splits the kernel stack arena. Runs once, after the frame
/// allocator is usable. Returns false when no 2 MiB-aligned run exists or the
/// TTBR1 block cannot be split; the caller must then fail the boot closed.
pub fn init() -> bool {
    let Some((base, generation)) =
        frame_allocator::alloc_contiguous_pages_aligned_tagged(ARENA_PAGES, ARENA_PAGES)
    else {
        return false;
    };

    let Some(table) = mmu::install_kernel_stack_arena(base) else {
        frame_allocator::free_contiguous_pages_tagged(base, ARENA_PAGES, generation);
        return false;
    };

    let mut arena = ARENA.lock();
    if arena.initialized {
        // init is a boot-time singleton; a second call must not leak the run
        // or replace the live table.
        drop(arena);
        frame_allocator::free_contiguous_pages_tagged(base, ARENA_PAGES, generation);
        return false;
    }
    arena.base = base;
    arena.table = table;
    arena.used = 0;
    arena.initialized = true;
    true
}

/// Allocates one guarded stack and returns the physical base of its four
/// usable pages. The guard page below that base stays unmapped.
pub fn alloc() -> Option<usize> {
    let mut arena = ARENA.lock();
    if !arena.initialized {
        return None;
    }
    let slot = (0..SLOT_LIMIT).find(|slot| arena.used & (1_u128 << slot) == 0)?;
    let stack = arena.base + slot * SLOT_PAGES * PAGE_SIZE + GUARD_PAGES * PAGE_SIZE;
    let first_index = slot * SLOT_PAGES + GUARD_PAGES;
    if !mmu::set_kernel_stack_pages(arena.table, first_index, stack, STACK_PAGES, true) {
        return None;
    }
    arena.used |= 1_u128 << slot;
    Some(stack)
}

/// Releases a stack previously returned by [`alloc`]. The pages are unmapped
/// before the slot becomes reusable, so a stale stack pointer cannot read or
/// write a frame that later belongs to another task.
pub fn free(stack: usize) -> bool {
    let mut arena = ARENA.lock();
    if !arena.initialized || stack < arena.base {
        return false;
    }
    let offset = stack - arena.base;
    if offset % (SLOT_PAGES * PAGE_SIZE) != GUARD_PAGES * PAGE_SIZE {
        return false;
    }
    let slot = offset / (SLOT_PAGES * PAGE_SIZE);
    if slot >= SLOT_LIMIT || arena.used & (1_u128 << slot) == 0 {
        return false;
    }
    let first_index = slot * SLOT_PAGES + GUARD_PAGES;
    if !mmu::set_kernel_stack_pages(arena.table, first_index, stack, STACK_PAGES, false) {
        return false;
    }
    arena.used &= !(1_u128 << slot);
    true
}

/// Boot-time proof that a slot maps its four pages, leaves the guard entry
/// invalid, and unmaps the pages again on free.
pub fn run_self_test() -> bool {
    let Some(stack) = alloc() else {
        return false;
    };
    let (table, slot) = {
        let arena = ARENA.lock();
        if !arena.initialized || stack < arena.base {
            return false;
        }
        let offset = stack - arena.base;
        (
            arena.table,
            offset / (SLOT_PAGES * PAGE_SIZE),
        )
    };

    let guard_index = slot * SLOT_PAGES;
    let guard_unmapped = !mmu::kernel_page_is_mapped(table, guard_index);
    let stack_mapped = (0..STACK_PAGES)
        .all(|index| mmu::kernel_page_is_mapped(table, guard_index + GUARD_PAGES + index));
    let freed = free(stack);
    let unmapped_after = (0..STACK_PAGES)
        .all(|index| !mmu::kernel_page_is_mapped(table, guard_index + GUARD_PAGES + index));
    guard_unmapped && stack_mapped && freed && unmapped_after
}

pub fn capacity() -> usize {
    let arena = ARENA.lock();
    if arena.initialized {
        SLOT_LIMIT
    } else {
        0
    }
}

pub fn free_slots() -> usize {
    let arena = ARENA.lock();
    if !arena.initialized {
        return 0;
    }
    SLOT_LIMIT - arena.used.count_ones() as usize
}

// Keep the slot arithmetic explicit and checked at compile time.
const _: () = assert!(SLOT_LIMIT * SLOT_PAGES <= ARENA_PAGES);
const _: () = assert!(SLOT_LIMIT <= 128);

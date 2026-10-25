//! Small first-fit `malloc`/`free` allocator over the task's `SYS_BRK` heap.
//!
//! Each task has a private copy of this state in its own ELF data. Free blocks
//! are sorted by address, split on allocation and coalesced on free. A free
//! block at the program break is returned to the kernel by shrinking `brk`.

use core::alloc::{GlobalAlloc, Layout};

use crate::syscall;

const ALIGNMENT: usize = 16;
const HEADER_SIZE: usize = core::mem::size_of::<BlockHeader>();
const MIN_FREE_BLOCK: usize = HEADER_SIZE + ALIGNMENT;

#[repr(C)]
struct BlockHeader {
    /// Total block size, including this header. Valid for allocated and free blocks.
    size: usize,
    /// Next free block; null while the block is allocated.
    next: *mut BlockHeader,
}

struct Allocator {
    free_head: *mut BlockHeader,
}

static mut ALLOCATOR: Allocator = Allocator {
    free_head: core::ptr::null_mut(),
};

/// `GlobalAlloc` adapter used by `Box`, `Vec` and other `alloc` collections.
/// The current backing malloc guarantees 16-byte alignment; stricter layouts
/// take the allocation-error path instead of returning a misaligned pointer.
pub struct UserAllocator;

unsafe impl GlobalAlloc for UserAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = if layout.align() <= ALIGNMENT {
            malloc(layout.size())
        } else {
            core::ptr::null_mut()
        };
        if pointer.is_null() {
            allocation_error(layout);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, _layout: Layout) {
        unsafe { free(pointer) };
    }
}

// `#[alloc_error_handler]` is still unstable. Intercepting null here gives a
// deterministic EL0 task exit and satisfies alloc's error path without it.
fn allocation_error(layout: Layout) -> ! {
    syscall::print("USER ALLOCATION ERROR\r\nSIZE=");
    syscall::print_dec_u64(layout.size() as u64);
    syscall::print("\r\nALIGN=");
    syscall::print_dec_u64(layout.align() as u64);
    syscall::print("\r\n");
    syscall::exit()
}

/// Allocates `size` bytes aligned to 16 bytes, or returns null on exhaustion.
/// A zero-sized allocation also returns null.
pub fn malloc(size: usize) -> *mut u8 {
    if size == 0 {
        return core::ptr::null_mut();
    }
    let Some(payload) = align_up(size) else {
        return core::ptr::null_mut();
    };
    let Some(requested) = payload.checked_add(HEADER_SIZE) else {
        return core::ptr::null_mut();
    };
    unsafe { allocator().allocate(requested) }
}

/// Returns a pointer previously obtained from `malloc` to the free list.
///
/// # Safety
/// `pointer` must be a live allocation from this allocator. Null is a no-op;
/// double frees, interior pointers and foreign pointers are invalid, as in C.
pub unsafe fn free(pointer: *mut u8) {
    if pointer.is_null() {
        return;
    }
    let block = unsafe { pointer.sub(HEADER_SIZE) as *mut BlockHeader };
    unsafe { allocator().deallocate(block) };
}

impl Allocator {
    fn allocate(&mut self, requested: usize) -> *mut u8 {
        let mut previous: *mut BlockHeader = core::ptr::null_mut();
        let mut current = self.free_head;

        while !current.is_null() {
            let (size, next) = unsafe { (core::ptr::read(current).size, (*current).next) };
            if size >= requested {
                let remainder = size - requested;
                let allocated_size = if remainder >= MIN_FREE_BLOCK {
                    let split = unsafe { (current as *mut u8).add(requested) as *mut BlockHeader };
                    unsafe {
                        core::ptr::write(
                            split,
                            BlockHeader {
                                size: remainder,
                                next,
                            },
                        );
                        if previous.is_null() {
                            self.free_head = split;
                        } else {
                            (*previous).next = split;
                        }
                    }
                    requested
                } else {
                    unsafe {
                        if previous.is_null() {
                            self.free_head = next;
                        } else {
                            (*previous).next = next;
                        }
                    }
                    size
                };
                return unsafe {
                    core::ptr::write(
                        current,
                        BlockHeader {
                            size: allocated_size,
                            next: core::ptr::null_mut(),
                        },
                    );
                    (current as *mut u8).add(HEADER_SIZE)
                };
            } else {
                previous = current;
                current = next;
                continue;
            }
        }

        self.allocate_from_kernel(requested)
    }

    fn allocate_from_kernel(&mut self, requested: usize) -> *mut u8 {
        let Some(current_break) = syscall::program_break() else {
            return core::ptr::null_mut();
        };
        let Some(new_break) = current_break.checked_add(requested) else {
            return core::ptr::null_mut();
        };
        if !syscall::set_program_break(new_break) {
            return core::ptr::null_mut();
        }

        let block = current_break as *mut BlockHeader;
        unsafe {
            core::ptr::write(
                block,
                BlockHeader {
                    size: requested,
                    next: core::ptr::null_mut(),
                },
            );
            (block as *mut u8).add(HEADER_SIZE)
        }
    }

    fn deallocate(&mut self, block: *mut BlockHeader) {
        let mut previous: *mut BlockHeader = core::ptr::null_mut();
        let mut current = self.free_head;
        while !current.is_null() && (current as usize) < (block as usize) {
            previous = current;
            current = unsafe { (*current).next };
        }

        unsafe {
            (*block).next = current;
            if previous.is_null() {
                self.free_head = block;
            } else {
                (*previous).next = block;
            }
            coalesce(block);
            if !previous.is_null() {
                coalesce(previous);
            }
        }
        self.trim_program_break();
    }

    /// Removes the last free block from the list when it ends exactly at brk.
    /// Shrinking first keeps the list valid if the kernel rejects the request.
    fn trim_program_break(&mut self) {
        let Some(current_break) = syscall::program_break() else {
            return;
        };
        let mut previous: *mut BlockHeader = core::ptr::null_mut();
        let mut last = self.free_head;
        if last.is_null() {
            return;
        }
        while unsafe { !(*last).next.is_null() } {
            previous = last;
            last = unsafe { (*last).next };
        }

        let last_end = unsafe { last as usize + (*last).size };
        if last_end != current_break || !syscall::set_program_break(last as usize) {
            return;
        }
        unsafe {
            if previous.is_null() {
                self.free_head = core::ptr::null_mut();
            } else {
                (*previous).next = core::ptr::null_mut();
            }
        }
    }
}

unsafe fn coalesce(block: *mut BlockHeader) {
    let next = unsafe { (*block).next };
    if !next.is_null() && unsafe { block as usize + (*block).size == next as usize } {
        unsafe {
            (*block).size += (*next).size;
            (*block).next = (*next).next;
        }
    }
}

fn align_up(size: usize) -> Option<usize> {
    size.checked_add(ALIGNMENT - 1)
        .map(|value| value & !(ALIGNMENT - 1))
}

unsafe fn allocator() -> &'static mut Allocator {
    unsafe { &mut *core::ptr::addr_of_mut!(ALLOCATOR) }
}

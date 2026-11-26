//! Memory milestone: the frame allocator, the boot framebuffer proof and the
//! heap.
//!
//! Each step is a precondition for the next: the allocator needs a window the
//! board description can vouch for, the framebuffer proof needs frames, and the
//! heap needs the MMU's mapping helpers the allocator already exercised.

use alloc::boxed::Box;

#[cfg(all(not(feature = "tablet"), not(feature = "fog")))]
use crate::boot_metrics;
use crate::{allocator, console, exceptions, frame_allocator, memory, ram_log};

/// Minimal boot framebuffer proof. On the phone the Redmi `cont_splash`
/// region is painted directly; on QEMU (and any non-fog profile) a small
/// frame-allocator surface validates the writer without a display.
fn simplefb_boot() {
    #[cfg(feature = "fog")]
    {
        let info = silver_microkernel::simplefb::FOG_PANEL;
        let Some(bytes) = info.bytes() else {
            return;
        };
        let pointer = memory::phys_to_virt(info.base as usize) as *mut u32;
        let pixels = unsafe { core::slice::from_raw_parts_mut(pointer, bytes / 4) };
        let Some(mut canvas) =
            silver_microkernel::simplefb::Canvas::new(pixels, info.width, info.height, info.stride)
        else {
            return;
        };
        canvas.paint_boot();
        console::write_str("SIMPLEFB: FOG base=0x");
        console::write_hex_u64(info.base);
        console::write_str(" 720x1650 stride=720\r\n");
        ram_log::log("SIMPLEFB FOG PANEL");
    }

    #[cfg(not(feature = "fog"))]
    {
        const WIDTH: usize = 128;
        const HEIGHT: usize = 96;
        const STRIDE: usize = 128;
        let pages = (WIDTH * HEIGHT * 4).div_ceil(memory::PAGE_SIZE);
        let Some((physical, generation)) = frame_allocator::alloc_contiguous_pages_tagged(pages)
        else {
            console::write_str("SIMPLEFB TEST: NO FRAMES\r\n");
            return;
        };
        let pointer = memory::phys_to_virt(physical) as *mut u32;
        let pixels = unsafe { core::slice::from_raw_parts_mut(pointer, STRIDE * HEIGHT) };
        let mut ok = false;
        let mut painted = false;
        if let Some(mut canvas) =
            silver_microkernel::simplefb::Canvas::new(pixels, WIDTH, HEIGHT, STRIDE)
        {
            canvas.paint_boot();
            painted = true;
        }
        if painted {
            ok = pixels[0] != 0 && pixels[STRIDE * (HEIGHT / 3) + WIDTH / 2] == 0xFF39_D2FF;
        }
        frame_allocator::free_contiguous_pages_tagged(physical, pages, generation);
        if ok {
            console::write_str("SIMPLEFB TEST PASS 128x96\r\n");
        } else {
            console::write_str("SIMPLEFB TEST FAIL\r\n");
        }
    }
}

/// Brings up the frame allocator, paints the boot framebuffer proof and the
/// heap. Failures stop the boot rather than continuing with a subsystem the
/// rest of the kernel would have to defend against.
///
/// Returns the heap base (for the fog banner) and a live heap allocation. The
/// caller keeps that allocation until the boot self-tests have run, so the
/// heap is provably still valid once they have exercised the allocator.
pub fn init() -> (usize, Box<u64>) {
    let memory_map = memory::current();
    if !memory_map.is_valid() || memory_map.available.is_empty() {
        #[cfg(feature = "fog")]
        console::write_str("SILVER FOG HALT: invalid memory map\r\n");
        exceptions::silver_handle_exception(6, 0, 0, 0);
    }
    let available_pages = frame_allocator::init(memory_map.available);

    if !memory_map.is_valid() || available_pages == 0 || !frame_allocator::run_self_test() {
        exceptions::silver_handle_exception(6, 0, 0, 0);
    }
    ram_log::log("FRAME ALLOC OK");
    simplefb_boot();

    // The heap comes before the filesystem: mounting validates the image with
    // `sfs_format::fsck`, which now tracks one bit per data block (a 16 MiB
    // rootfs has more blocks than a single file's `MAX_FILE_BLOCKS` window and
    // no longer fits a fixed stack array).
    let Some(heap_base) = allocator::init() else {
        exceptions::silver_handle_exception(7, 0, 0, 0);
    };

    let value = Box::new(42_u64);
    #[cfg(all(not(feature = "tablet"), not(feature = "fog")))]
    {
        let value_address = (&*value as *const u64) as usize;
        console::write_str("HEAP=OK BASE=");
        console::write_hex_u64(heap_base as u64);
        console::write_str(" VALUE=");
        console::write_hex_u64(*value);
        console::write_str(" ADDR=");
        console::write_hex_u64(value_address as u64);
        console::write_str("\r\n");
        boot_metrics::log_milestone("Heap init");
    }
    ram_log::log("HEAP OK");
    crate::log_info!(
        "boot",
        "heap ready base=0x{:x} test_value={}",
        heap_base as u64,
        *value
    );
    (heap_base, value)
}

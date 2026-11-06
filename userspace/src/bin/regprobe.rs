//! regprobe: checks that the kernel preserves the caller-saved registers
//! (x9-x17) and the callee-saved ones (x19-x29) across an `svc`.
//!
//! Exit status bitmap: bit 0..8 = x9..x17, bit 9..19 = x19..x29.

#![no_std]
#![no_main]

#[path = "../ipc.rs"]
mod ipc;
#[path = "../syscall.rs"]
mod syscall;

use core::panic::PanicInfo;

static SENTINELS: [u64; 20] = [
    0x0909_0909_0909_0909, // x9
    0x1010_1010_1010_1010, // x10
    0x1111_1111_1111_1111, // x11
    0x1212_1212_1212_1212, // x12
    0x1313_1313_1313_1313, // x13
    0x1414_1414_1414_1414, // x14
    0x1515_1515_1515_1515, // x15
    0x1616_1616_1616_1616, // x16
    0x1717_1717_1717_1717, // x17
    0x1919_1919_1919_1919, // x19
    0x2020_2020_2020_2020, // x20
    0x2121_2121_2121_2121, // x21
    0x2222_2222_2222_2222, // x22
    0x2323_2323_2323_2323, // x23
    0x2424_2424_2424_2424, // x24
    0x2525_2525_2525_2525, // x25
    0x2626_2626_2626_2626, // x26
    0x2727_2727_2727_2727, // x27
    0x2828_2828_2828_2828, // x28
    0x2929_2929_2929_2929, // x29
];

static mut OBSERVED: [u64; 20] = [0; 20];

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(_argument: u64) -> ! {
    let source = SENTINELS.as_ptr();
    let output = core::ptr::addr_of_mut!(OBSERVED) as *mut u64;
    unsafe {
        core::arch::asm!(
            // x19 is known to survive syscalls (tested separately), so it keeps
            // the output pointer while x0-x17 are under test.
            "mov x19, {out}",
            "ldp x9, x10, [{src}, #0]",
            "ldp x11, x12, [{src}, #16]",
            "ldp x13, x14, [{src}, #32]",
            "ldp x15, x16, [{src}, #48]",
            "ldr x17, [{src}, #64]",
            "mov x8, #7", // SYS_WHOAMI
            "svc #0",
            "stp x9, x10, [x19, #0]",
            "stp x11, x12, [x19, #16]",
            "stp x13, x14, [x19, #32]",
            "stp x15, x16, [x19, #48]",
            "str x17, [x19, #64]",
            src = in(reg) source,
            out = in(reg) output,
            options(nostack),
        );

        // Callee-saved group, reusing the same pattern with x19 as source and
        // x0 as the (post-svc) destination pointer: x0 is not under test.
        core::arch::asm!(
            "ldp x19, x20, [{src}, #72]",
            "ldp x21, x22, [{src}, #88]",
            "ldp x23, x24, [{src}, #104]",
            "ldp x25, x26, [{src}, #120]",
            "ldp x27, x28, [{src}, #136]",
            "ldr x29, [{src}, #152]",
            "mov x8, #7",
            "svc #0",
            "stp x19, x20, [{out}, #72]",
            "stp x21, x22, [{out}, #88]",
            "stp x23, x24, [{out}, #104]",
            "stp x25, x26, [{out}, #120]",
            "stp x27, x28, [{out}, #136]",
            "str x29, [{out}, #152]",
            src = in(reg) source,
            out = in(reg) output,
            in("x8") 7u64,
            lateout("x0") _,
            options(nostack),
        );
    }

    let observed = unsafe { &*core::ptr::addr_of!(OBSERVED) };
    let mut lost = 0u64;
    for index in 0..20 {
        if observed[index] != SENTINELS[index] {
            lost |= 1 << index;
        }
    }
    syscall::exit_with_status(lost);
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) }
    }
}

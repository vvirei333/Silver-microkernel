#![no_std]
#![no_main]

#[path = "../ipc.rs"]
mod ipc;
#[path = "../syscall.rs"]
mod syscall;

use core::panic::PanicInfo;

const TRAIN: [&str; 8] = [
    r"       ====        ________                ___________ ",
    r"   _D _|  |_______/        \__I_I_____===__|_________| ",
    r"    |(_)---  |   H\________/ |   |        =|___ ___|   ",
    r"    /     |  |   H  |  |     |   |         ||_| |_||   ",
    r"   |      |  |   H  |__--------------------| [___] |   ",
    r" __/ =| o |=-~~\  /~~\  /~~\  /~~\ ____Y___________|_| ",
    r"  |/__=|___|=    ||    ||    ||    |_____/~|\___/      ",
    r"   \_/      \__/  \__/  \__/  \__/      \_/            ",
];

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(_argument: u64) -> ! {
    for offset in 0..60 {
        syscall::print("\x1b[2J\x1b[H");
        for line in TRAIN {
            print_spaces(offset);
            syscall::print(line);
            syscall::print("\r\n");
        }
        syscall::sleep(60);
    }
    syscall::exit();
}

fn print_spaces(mut count: usize) {
    const SPACES: &str = "                        ";
    while count > 0 {
        let length = count.min(SPACES.len());
        syscall::print(&SPACES[..length]);
        count -= length;
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
    }
}

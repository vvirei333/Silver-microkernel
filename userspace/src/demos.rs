use crate::{ipc, syscall};

const PRIVATE_WINDOW_VA: usize = 0x0000_0080_0000_0000;
const HIGH_HALF_DRAM: usize = 0xFFFF_0000_4000_0000;

pub fn mutex_holder() -> ! {
    loop {
        syscall::mutex_lock();
        syscall::print("MUTEX HOLDER=1 ACQUIRE\r\n");
        syscall::sleep(400);
        syscall::print("MUTEX HOLDER=1 RELEASE\r\n");
        syscall::mutex_unlock();
        syscall::sleep(50);
    }
}

pub fn mutex_contender() -> ! {
    loop {
        syscall::mutex_lock();
        syscall::print("MUTEX T");
        syscall::print_dec_u64(syscall::task_id());
        syscall::print(" LOCK\r\n");
        syscall::mutex_unlock();
        syscall::sleep(100);
    }
}

fn resolve_peer(name: &str, peer: &str) -> u64 {
    syscall::print("IPC REGISTER ");
    syscall::print(name);
    syscall::print("\r\n");
    if !syscall::register(name) {
        syscall::print("IPC REGISTER FAILED\r\n");
    }
    loop {
        if let Some(id) = syscall::lookup(peer) {
            syscall::print("IPC FOUND ");
            syscall::print(peer);
            syscall::print("\r\n");
            return id;
        }
        syscall::print("IPC LOOKUP ");
        syscall::print(peer);
        syscall::print(" RETRY\r\n");
        syscall::sleep(100);
    }
}

pub fn ipc_ping() -> ! {
    let peer = resolve_peer("ping", "pong");
    let mut reply = ipc::Message::new(0, b"");
    loop {
        syscall::print("IPC ping SEND PING (SVC)\r\n");
        syscall::send(peer, &ipc::Message::new(syscall::task_id(), b"PING"));
        syscall::receive(&mut reply);
        syscall::print("IPC ping RECV ");
        syscall::print(core::str::from_utf8(reply.text()).unwrap_or("?"));
        syscall::print(" (SVC)\r\n");
        syscall::sleep(300);
    }
}

pub fn ipc_pong() -> ! {
    resolve_peer("pong", "ping");
    let mut message = ipc::Message::new(0, b"");
    loop {
        syscall::sleep(200);
        syscall::receive(&mut message);
        syscall::print("IPC pong RECV ");
        syscall::print(core::str::from_utf8(message.text()).unwrap_or("?"));
        syscall::print(" (SVC)\r\n");
        syscall::print("IPC pong SEND PONG (SVC)\r\n");
        syscall::send(
            message.sender,
            &ipc::Message::new(syscall::task_id(), b"PONG"),
        );
    }
}

pub fn sleeping_task() -> ! {
    loop {
        syscall::sleep(500);
        syscall::print("TASK ID=");
        syscall::print_hex_u64(syscall::task_id());
        syscall::print(" AWAKE\r\n");
    }
}

pub fn condvar_alpha() -> ! {
    syscall::register("alpha");
    let mut message = ipc::Message::new(0, b"");
    loop {
        syscall::receive(&mut message);
        syscall::print("CONDVAR alpha RECV ");
        syscall::print(core::str::from_utf8(message.text()).unwrap_or("?"));
        syscall::print("\r\n");
    }
}

pub fn condvar_beta() -> ! {
    syscall::register("beta");
    let mut message = ipc::Message::new(0, b"");
    loop {
        syscall::receive(&mut message);
        syscall::print("CONDVAR beta RECV (TEST FAIL)\r\n");
    }
}

pub fn condvar_probe() -> ! {
    syscall::register("probe");
    let alpha = loop {
        if let Some(id) = syscall::lookup("alpha") {
            break id;
        }
        syscall::sleep(50);
    };
    loop {
        if syscall::lookup("beta").is_some() {
            break;
        }
        syscall::sleep(50);
    }
    syscall::sleep(300);
    syscall::print("CONDVAR TEST: send WAKE to alpha only\r\n");
    syscall::send(alpha, &ipc::Message::new(syscall::task_id(), b"WAKE"));
    syscall::sleep(500);
    syscall::print("CONDVAR TEST: beta silent = PASS\r\n");
    loop {
        syscall::sleep(1000);
    }
}

pub fn channel_producer() -> ! {
    loop {
        for item in 1..=5_u64 {
            syscall::channel_send(item);
            syscall::print("CHAN TX ");
            syscall::print_dec_u64(item);
            syscall::print("\r\n");
        }
    }
}

pub fn channel_consumer() -> ! {
    loop {
        syscall::sleep(400);
        let item = syscall::channel_receive();
        syscall::print("CHAN RX ");
        syscall::print_dec_u64(item);
        syscall::print("\r\n");
    }
}

static mut ELF_BSS_CELL: u64 = 0;

pub fn vm_task_a() -> ! {
    vm_task(b'X', 0xAAAA)
}

pub fn vm_task_b() -> ! {
    vm_task(b'Y', 0xBBBB)
}

fn vm_task(label: u8, value: u64) -> ! {
    let cell = PRIVATE_WINDOW_VA as *mut u64;
    let elf_cell = core::ptr::addr_of_mut!(ELF_BSS_CELL);
    loop {
        unsafe {
            core::ptr::write_volatile(cell, value);
            core::ptr::write_volatile(elf_cell, value);
        }
        syscall::sleep(500);
        let observed = unsafe { core::ptr::read_volatile(cell) };
        let elf_observed = unsafe { core::ptr::read_volatile(elf_cell) };
        if observed != value || elf_observed != value {
            syscall::print("VM ISOLATION FAIL\r\n");
        }
        if label == b'X' {
            syscall::print("VM A [0x");
        } else {
            syscall::print("VM B [0x");
        }
        syscall::print_hex_u64(PRIVATE_WINDOW_VA as u64);
        syscall::print("]=0x");
        syscall::print_hex_u64(observed);
        syscall::print(" ELF=0x");
        syscall::print_hex_u64(elf_observed);
        syscall::print("\r\n");
    }
}

pub fn isolation_probe() -> ! {
    syscall::print("ISOLATION PROBE: reading high-half kernel memory\r\n");
    let value = unsafe { core::ptr::read_volatile(HIGH_HALF_DRAM as *const u64) };
    syscall::print("ISOLATION FAIL: kernel read returned 0x");
    syscall::print_hex_u64(value);
    syscall::print("\r\n");
    loop {
        syscall::sleep(1000);
    }
}

pub fn kernel_image_probe() -> ! {
    syscall::print("ELF ALIAS PROBE: reading old kernel text VA\r\n");
    let value = unsafe { core::ptr::read_volatile(0x4008_0000 as *const u64) };
    syscall::print("ELF ALIAS FAIL: kernel text read returned 0x");
    syscall::print_hex_u64(value);
    syscall::print("\r\n");
    loop {
        syscall::sleep(1000);
    }
}

pub fn unknown_task() -> ! {
    syscall::print("UNKNOWN USER TASK\r\n");
    loop {
        syscall::sleep(1000);
    }
}

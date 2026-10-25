use crate::{allocator, ipc, syscall};

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
    let heap_cell = syscall::USER_HEAP_VA as *mut u64;
    if !syscall::set_program_break(syscall::USER_HEAP_VA + 4096) {
        syscall::print("VM HEAP MAP FAIL\r\n");
        syscall::exit();
    }
    loop {
        unsafe {
            core::ptr::write_volatile(cell, value);
            core::ptr::write_volatile(elf_cell, value);
            core::ptr::write_volatile(heap_cell, value);
        }
        syscall::sleep(500);
        let observed = unsafe { core::ptr::read_volatile(cell) };
        let elf_observed = unsafe { core::ptr::read_volatile(elf_cell) };
        let heap_observed = unsafe { core::ptr::read_volatile(heap_cell) };
        if observed != value || elf_observed != value || heap_observed != value {
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
        syscall::print(" HEAP=0x");
        syscall::print_hex_u64(heap_observed);
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

pub fn lifecycle_supervisor() -> ! {
    const NAME: &str = "lifecycle";
    syscall::sleep(1000);
    syscall::print("LIFECYCLE SPAWN demo arg=18\r\n");
    let Some(child) = syscall::spawn(syscall::PROGRAM_DEMO, 18).and_then(syscall::cap_get_task_id)
    else {
        syscall::print("LIFECYCLE SPAWN=FAIL\r\n");
        loop {
            syscall::sleep(1000);
        }
    };
    syscall::print("LIFECYCLE CHILD ID=");
    syscall::print_dec_u64(child);
    syscall::print("\r\n");

    let mut saw_registration = false;
    let mut exited = false;
    for _ in 0..100 {
        if syscall::lookup(NAME) == Some(child) {
            saw_registration = true;
        }
        if saw_registration && !syscall::task_alive(child) {
            exited = true;
            break;
        }
        syscall::sleep(50);
    }

    if !saw_registration || !exited || syscall::lookup(NAME).is_some() {
        syscall::print("LIFECYCLE EXIT/UNREGISTER=FAIL\r\n");
    } else {
        syscall::print("LIFECYCLE EXIT/UNREGISTER=PASS\r\n");
    }

    // The child intentionally exits while owning the demo mutex. Reaping
    // must release it and wake a waiter instead of deadlocking the kernel.
    syscall::mutex_lock();
    syscall::print("LIFECYCLE MUTEX CLEANUP=PASS\r\n");
    syscall::mutex_unlock();
    loop {
        syscall::sleep(1000);
    }
}

pub fn lifecycle_child() -> ! {
    syscall::print("LIFECYCLE CHILD ARG=18\r\n");
    if !syscall::register("lifecycle") {
        syscall::print("LIFECYCLE CHILD REGISTER=FAIL\r\n");
    }
    syscall::sleep(500);
    if !syscall::unregister("lifecycle") {
        syscall::print("LIFECYCLE CHILD UNREGISTER=FAIL\r\n");
    }
    syscall::mutex_lock();
    syscall::print("LIFECYCLE CHILD EXIT HOLDING MUTEX\r\n");
    syscall::exit()
}

static READ_ONLY_MESSAGE: ipc::Message = ipc::Message {
    sender: 0,
    payload: [0; ipc::PAYLOAD_SIZE],
};

pub fn copyout_probe() -> ! {
    let read_only = &READ_ONLY_MESSAGE as *const ipc::Message as *mut ipc::Message;
    if unsafe { syscall::receive_raw(read_only) } {
        syscall::print("COPYOUT READONLY REJECT=FAIL\r\n");
    } else {
        syscall::print("COPYOUT READONLY REJECT=PASS\r\n");
    }
    if unsafe { syscall::receive_raw(0x40 as *mut ipc::Message) } {
        syscall::print("COPYOUT UNMAPPED REJECT=FAIL\r\n");
    } else {
        syscall::print("COPYOUT UNMAPPED REJECT=PASS\r\n");
    }
    syscall::exit()
}

pub fn heap_probe() -> ! {
    const PAGE: usize = 4096;
    let base = syscall::USER_HEAP_VA;
    let expanded = base + 3 * PAGE;
    let bytes = base as *mut u8;

    if syscall::program_break() != Some(base) || !syscall::set_program_break(expanded) {
        syscall::print("HEAP INITIAL GROW=FAIL\r\n");
        syscall::exit();
    }
    unsafe {
        for page in 0..3 {
            if core::ptr::read_volatile(bytes.add(page * PAGE)) != 0
                || core::ptr::read_volatile(bytes.add((page + 1) * PAGE - 1)) != 0
            {
                syscall::print("HEAP ZERO FILL=FAIL\r\n");
                syscall::exit();
            }
            core::ptr::write_volatile(bytes.add((page + 1) * PAGE - 1), 0x11 + page as u8);
        }
    }

    if !syscall::set_program_break(base + PAGE)
        || syscall::program_break() != Some(base + PAGE)
        || !syscall::set_program_break(expanded)
    {
        syscall::print("HEAP SHRINK/REGROW=FAIL\r\n");
        syscall::exit();
    }
    let contents_ok = unsafe {
        core::ptr::read_volatile(bytes.add(PAGE - 1)) == 0x11
            && core::ptr::read_volatile(bytes.add(2 * PAGE - 1)) == 0
            && core::ptr::read_volatile(bytes.add(3 * PAGE - 1)) == 0
    };
    if !contents_ok {
        syscall::print("HEAP CONTENTS=FAIL\r\n");
        syscall::exit();
    }
    if syscall::set_program_break(base - 1)
        || syscall::set_program_break(base + syscall::USER_HEAP_MAX_SIZE + 1)
        || syscall::program_break() != Some(expanded)
    {
        syscall::print("HEAP BOUNDS=FAIL\r\n");
        syscall::exit();
    }

    syscall::print("HEAP GROW/SHRINK=PASS\r\n");
    syscall::exit()
}

pub fn malloc_probe() -> ! {
    let Some(initial_break) = syscall::program_break() else {
        syscall::print("MALLOC BRK QUERY=FAIL\r\n");
        syscall::exit();
    };
    let a = allocator::malloc(24);
    let b = allocator::malloc(64);
    let c = allocator::malloc(128);
    if a.is_null()
        || b.is_null()
        || c.is_null()
        || a as usize & 15 != 0
        || b as usize & 15 != 0
        || c as usize & 15 != 0
        || !allocator::malloc(0).is_null()
    {
        syscall::print("MALLOC ALLOCATION=FAIL\r\n");
        syscall::exit();
    }

    unsafe {
        for index in 0..24 {
            core::ptr::write_volatile(a.add(index), 0xA0 + (index % 16) as u8);
        }
        for index in 0..64 {
            core::ptr::write_volatile(b.add(index), 0xB0 + (index % 16) as u8);
        }
        for index in 0..128 {
            core::ptr::write_volatile(c.add(index), 0xC0 + (index % 8) as u8);
        }
        allocator::free(b);
    }

    // The freed 64-byte payload block is large enough to split. First-fit must
    // return its original address while leaving a new free remainder.
    let recycled = allocator::malloc(32);
    let neighbors_intact = unsafe {
        (0..24).all(|index| core::ptr::read_volatile(a.add(index)) == 0xA0 + (index % 16) as u8)
            && (0..128)
                .all(|index| core::ptr::read_volatile(c.add(index)) == 0xC0 + (index % 8) as u8)
    };
    if recycled != b || !neighbors_intact {
        syscall::print("MALLOC FREE/REUSE=FAIL\r\n");
        syscall::exit();
    }
    syscall::print("MALLOC FREE/REUSE=PASS\r\n");

    // Sorted insertion plus coalescing merges A, the recycled/split B area and
    // C into one tail block, which trim_program_break returns through SYS_BRK.
    unsafe {
        allocator::free(recycled);
        allocator::free(a);
        allocator::free(c);
    }
    if syscall::program_break() != Some(initial_break) {
        syscall::print("MALLOC BRK TRIM=FAIL\r\n");
        syscall::exit();
    }
    syscall::print("MALLOC BRK TRIM=PASS\r\n");
    syscall::exit()
}

pub fn global_alloc_probe() -> ! {
    let Some(initial_break) = syscall::program_break() else {
        syscall::print("GLOBAL ALLOC BRK QUERY=FAIL\r\n");
        syscall::exit();
    };

    let boxed = alloc::boxed::Box::new(42_u64);
    let mut values = alloc::vec::Vec::new();
    // Vec grows through several capacities, exercising GlobalAlloc's default
    // realloc path (allocate, copy, deallocate) as well as fresh allocations.
    for value in 0..32_u64 {
        values.push(value * 3);
    }
    let contents_ok = values
        .iter()
        .copied()
        .eq((0..32_u64).map(|value| value * 3));
    if *boxed != 42 || values.len() != 32 || !contents_ok {
        syscall::print("GLOBAL ALLOC BOX/VEC=FAIL\r\n");
        syscall::exit();
    }
    syscall::print("GLOBAL ALLOC BOX=42 VEC_LEN=32 PASS\r\n");

    drop(values);
    drop(boxed);
    if syscall::program_break() != Some(initial_break) {
        syscall::print("GLOBAL ALLOC TRIM=FAIL\r\n");
        syscall::exit();
    }
    syscall::print("GLOBAL ALLOC TRIM=PASS\r\n");
    syscall::exit()
}

fn wait_for_name(name: &str) -> u64 {
    loop {
        if let Some(id) = syscall::lookup(name) {
            return id;
        }
        syscall::sleep(20);
    }
}

pub fn ipc_backpressure_sender() -> ! {
    if !syscall::register("bp-a") {
        syscall::print("IPC BP SENDER REGISTER=FAIL\r\n");
        syscall::exit();
    }
    let receiver = wait_for_name("bp-b");
    wait_for_name("bp-ready");
    let id = syscall::task_id();
    for sequence in 0..16_u8 {
        let mut message = ipc::Message::new(id, b"");
        message.payload[0] = sequence;
        if !syscall::send(receiver, &message) {
            syscall::print("IPC BP FILL=FAIL\r\n");
            syscall::exit();
        }
    }
    syscall::print("IPC BP QUEUE FULL\r\n");

    let mut seventeenth = ipc::Message::new(id, b"");
    seventeenth.payload[0] = 16;
    if syscall::send(receiver, &seventeenth) {
        syscall::print("IPC BP SENDER RESUMED=PASS\r\n");
    } else {
        syscall::print("IPC BP SENDER RESUMED=FAIL\r\n");
    }
    syscall::exit()
}

pub fn ipc_backpressure_receiver() -> ! {
    if !syscall::register("bp-b") {
        syscall::print("IPC BP REGISTER=FAIL\r\n");
        syscall::exit();
    }

    let mut message = ipc::Message::new(0, b"");
    match syscall::receive_timeout(&mut message, 5) {
        Err(syscall::ReceiveError::Timeout) => syscall::print("IPC TIMEOUT=PASS\r\n"),
        _ => syscall::print("IPC TIMEOUT=FAIL\r\n"),
    }
    if !syscall::register("bp-ready") {
        syscall::print("IPC BP READY=FAIL\r\n");
        syscall::exit();
    }

    // Let A fill all 16 slots and block on the seventeenth send. The first
    // receive below frees one slot and must wake exactly that sender.
    syscall::sleep(500);
    let mut sender = None;
    let mut valid = true;
    for sequence in 0..17_u8 {
        match syscall::receive_timeout(&mut message, 1000) {
            Ok(()) => {
                let expected_sender = *sender.get_or_insert(message.sender);
                valid &= message.sender == expected_sender && message.payload[0] == sequence;
            }
            Err(_) => {
                valid = false;
                break;
            }
        }
    }
    if valid {
        syscall::print("IPC BP BACKPRESSURE=PASS\r\n");
    } else {
        syscall::print("IPC BP BACKPRESSURE=FAIL\r\n");
    }
    syscall::exit()
}

pub fn ipc_exit_sender() -> ! {
    let receiver = wait_for_name("ipc-gone");
    let id = syscall::task_id();
    for sequence in 0..17_u8 {
        let mut message = ipc::Message::new(id, b"");
        message.payload[0] = sequence;
        let sent = syscall::send(receiver, &message);
        if sequence < 16 && !sent {
            syscall::print("IPC EXIT FILL=FAIL\r\n");
            syscall::exit();
        }
        if sequence == 16 {
            if sent {
                syscall::print("IPC EXIT WAKE=FAIL\r\n");
            } else {
                syscall::print("IPC EXIT WAKE=PASS\r\n");
            }
            syscall::exit();
        }
    }
    syscall::exit()
}

pub fn ipc_exit_receiver() -> ! {
    if !syscall::register("ipc-gone") {
        syscall::print("IPC EXIT RECEIVER REGISTER=FAIL\r\n");
        syscall::exit();
    }
    // Do not issue another RPC here: once the queue is full, even a nameserver
    // reply could not fit. Give the sender time to block on message 17, then exit
    // without draining; cleanup must discard the queue and wake it with an error.
    syscall::sleep(1000);
    syscall::exit()
}

pub fn security_probe() -> ! {
    let id = syscall::task_id();
    let spoofed = ipc::Message::new(1, b"SPOOF");
    syscall::send(id, &spoofed);
    let mut received = ipc::Message::new(0, b"");
    syscall::receive(&mut received);
    if received.sender == id && received.text() == b"SPOOF" {
        syscall::print("SECURITY IPC SENDER=PASS\r\n");
    } else {
        syscall::print("SECURITY IPC SENDER=FAIL\r\n");
    }

    if !syscall::mutex_unlock() {
        syscall::print("SECURITY MUTEX NONOWNER=PASS\r\n");
    } else {
        syscall::print("SECURITY MUTEX NONOWNER=FAIL\r\n");
    }
    syscall::mutex_lock();
    if syscall::mutex_unlock() {
        syscall::print("SECURITY MUTEX OWNER=PASS\r\n");
    } else {
        syscall::print("SECURITY MUTEX OWNER=FAIL\r\n");
    }

    if let Some(child_cap) = syscall::spawn(syscall::PROGRAM_DEMO, crate::ROLE_SLEEP_OVERFLOW) {
        if let Some(child) = syscall::cap_get_task_id(child_cap) {
            syscall::sleep(100);
            if syscall::task_alive(child)
                && syscall::kill(child_cap)
                && syscall::wait(child_cap) == Some(1)
            {
                syscall::print("SECURITY SLEEP OVERFLOW=PASS\r\n");
            } else {
                syscall::print("SECURITY SLEEP OVERFLOW=FAIL\r\n");
            }
        } else {
            syscall::print("SECURITY SLEEP OVERFLOW=FAIL\r\n");
        }
    } else {
        syscall::print("SECURITY SLEEP OVERFLOW=FAIL\r\n");
    }
    syscall::exit();
}

pub fn sleep_overflow() -> ! {
    syscall::sleep(u64::MAX);
    syscall::print("SECURITY SLEEP OVERFLOW=FAIL\r\n");
    syscall::exit();
}

pub fn unknown_task() -> ! {
    syscall::print("UNKNOWN USER TASK\r\n");
    loop {
        syscall::sleep(1000);
    }
}

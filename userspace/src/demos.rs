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

/// Marker written into a waitpid status buffer before the call. A waiter that
/// loses the race must still see this value afterwards, proving ECHILD did not
/// leak a status into its memory.
const WAIT_SENTINEL: u64 = 0xA5A5_A5A5_A5A5_A5A5;

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

/// End-to-end test for the versioned process-management API:
/// `SYS_SPAWN_ARGSV` (argv/envp block), `SYS_GETPPID`, `SYS_WAITPID`, one-shot
/// zombie consumption (repeat wait -> ECHILD), and denial of a foreign PID.
/// The child validates its non-zero PPID and exits with status 7.
pub fn process_api_probe() -> ! {
    // Let boot settle so the child does not race task creation.
    syscall::sleep(300);
    if syscall::getppid() == 0 {
        syscall::print("PROC BOOT PPID=PASS\r\n");
    } else {
        syscall::print("PROC BOOT PPID=FAIL\r\n");
    }

    let Some(cap) = syscall::spawn_with_argsv(
        syscall::PROGRAM_DEMO,
        crate::ROLE_PROCESS_API_CHILD,
        &["proc-child", "one", "two"],
        &["MODE=test", "EMPTY="],
    ) else {
        syscall::print("PROC SPAWN ARGSV=FAIL\r\n");
        loop {
            syscall::sleep(1000);
        }
    };
    syscall::print("PROC SPAWN ARGSV=PASS\r\n");

    let Some(child) = syscall::cap_get_task_id(cap) else {
        syscall::print("PROC CHILD ID=FAIL\r\n");
        loop {
            syscall::sleep(1000);
        }
    };

    // The CAP_ALL spawn handle authorises operations *on the child*; it must
    // not become an own device/mount privilege of the parent.
    let _ = syscall::fs_mkdir("/tmp/proc-handle");
    let mount_leak = syscall::fs_mount("/", "/tmp/proc-handle", "ramfs", 0);
    let _ = syscall::fs_rmdir("/tmp/proc-handle");
    if mount_leak {
        syscall::print("SECURITY SPAWN HANDLE MOUNT DENY=FAIL\r\n");
    } else {
        syscall::print("SECURITY SPAWN HANDLE MOUNT DENY=PASS\r\n");
    }
    if syscall::alloc_dma(4096).is_none() {
        syscall::print("SECURITY SPAWN HANDLE DMA DENY=PASS\r\n");
    } else {
        syscall::print("SECURITY SPAWN HANDLE DMA DENY=FAIL\r\n");
    }

    // T15-02: the request is all-or-nothing. The handle covers CAP_KILL and
    // CAP_MMIO, but the parent owns no self-CAP_MMIO, so the mixed delegation
    // must be rejected as a whole (no partial CAP_MMIO on the recipient).
    let own = syscall::task_id();
    let mixed = syscall::cap_grant(cap, own, syscall::CAP_KILL | syscall::CAP_MMIO);
    if !mixed && syscall::alloc_dma(4096).is_none() {
        syscall::print("SECURITY MIXED GRANT DENY=PASS\r\n");
    } else {
        syscall::print("SECURITY MIXED GRANT DENY=FAIL\r\n");
    }
    // Same handle, target-scoped bit only: delegation still works.
    if syscall::cap_grant(cap, own, syscall::CAP_KILL) {
        syscall::print("SECURITY SCOPED GRANT=PASS\r\n");
    } else {
        syscall::print("SECURITY SCOPED GRANT=FAIL\r\n");
    }
    // Same handle, global bit only: still denied (handle is not ownership).
    if !syscall::cap_grant(cap, own, syscall::CAP_MMIO) {
        syscall::print("SECURITY GLOBAL GRANT DENY=PASS\r\n");
    } else {
        syscall::print("SECURITY GLOBAL GRANT DENY=FAIL\r\n");
    }

    // A failing copyout must leave the zombie available for a retry.
    if syscall::waitpid_raw(child, 0x40) != 0 {
        syscall::print("PROC WAITPID EFAULT=PASS\r\n");
    } else {
        syscall::print("PROC WAITPID EFAULT=FAIL\r\n");
    }
    let read_only = &READ_ONLY_MESSAGE as *const ipc::Message as u64;
    if syscall::waitpid_raw(child, read_only) != 0 {
        syscall::print("PROC WAITPID READONLY=PASS\r\n");
    } else {
        syscall::print("PROC WAITPID READONLY=FAIL\r\n");
    }
    match syscall::waitpid(child) {
        Some(7) => syscall::print("PROC WAITPID STATUS=PASS\r\n"),
        _ => syscall::print("PROC WAITPID STATUS=FAIL\r\n"),
    }
    match syscall::waitpid(child) {
        None => syscall::print("PROC REPEAT ECHILD=PASS\r\n"),
        Some(_) => syscall::print("PROC REPEAT ECHILD=FAIL\r\n"),
    }

    // status_ptr == 0 keeps the "consume without copyout" semantics.
    if let Some(cap2) = syscall::spawn(syscall::PROGRAM_DEMO, crate::ROLE_PROCESS_API_CHILD) {
        if let Some(child2) = syscall::cap_get_task_id(cap2) {
            if syscall::waitpid_raw(child2, 0) == 0 {
                syscall::print("PROC WAITPID NULL=PASS\r\n");
            } else {
                syscall::print("PROC WAITPID NULL=FAIL\r\n");
            }
            match syscall::waitpid(child2) {
                None => syscall::print("PROC WAITPID NULL ECHILD=PASS\r\n"),
                Some(_) => syscall::print("PROC WAITPID NULL ECHILD=FAIL\r\n"),
            }
        }
    }

    concurrent_wait_probe();

    // PID 1 (Scout in demo boot) is neither our child nor covered by a
    // capability; a foreign wait must be denied without blocking.
    match syscall::waitpid(1) {
        None => syscall::print("PROC FOREIGN DENY=PASS\r\n"),
        Some(_) => syscall::print("PROC FOREIGN DENY=FAIL\r\n"),
    }
    loop {
        syscall::sleep(1000);
    }
}

/// Two waiters race for one zombie: exactly one must get the status, the
/// other must see ECHILD *and* keep its status buffer untouched. The second
/// waiter is authorised through an explicit CAP_WAIT delegation on the target
/// child (target-scoped capability). Both sides use a sentinel-initialised
/// buffer so a failed consume cannot silently write a status.
fn concurrent_wait_probe() {
    let Some(waiter_cap) = syscall::spawn(syscall::PROGRAM_DEMO, crate::ROLE_CONCURRENT_WAITER) else {
        syscall::print("PROC CONCURRENT WAIT=FAIL\r\n");
        return;
    };
    let Some(waiter) = syscall::cap_get_task_id(waiter_cap) else {
        syscall::print("PROC CONCURRENT WAIT=FAIL\r\n");
        return;
    };
    let Some(child_cap) = syscall::spawn(syscall::PROGRAM_DEMO, crate::ROLE_PROCESS_API_CHILD) else {
        syscall::print("PROC CONCURRENT WAIT=FAIL\r\n");
        return;
    };
    let Some(child) = syscall::cap_get_task_id(child_cap) else {
        syscall::print("PROC CONCURRENT WAIT=FAIL\r\n");
        return;
    };
    if !syscall::cap_grant(child_cap, waiter, syscall::CAP_WAIT) {
        syscall::print("PROC CONCURRENT WAIT=FAIL\r\n");
        return;
    }
    // Hand the target PID to the waiter; it blocks in receive until now, so
    // the CAP_WAIT grant above is already in place when it starts waiting.
    let mut go = ipc::Message::new(0, b"");
    go.payload[..8].copy_from_slice(&child.to_le_bytes());
    if !syscall::send(waiter, &go) {
        syscall::print("PROC CONCURRENT WAIT=FAIL\r\n");
        return;
    }
    let mut parent_status = WAIT_SENTINEL;
    let parent_result = syscall::waitpid_raw(child, &mut parent_status as *mut u64 as u64);
    let mut reply = ipc::Message::new(0, b"");
    let (waiter_won, waiter_lost_clean) = match syscall::receive_timeout(&mut reply, 500) {
        Ok(()) if reply.sender == waiter => (reply.payload[0] == 1, reply.payload[1] == 1),
        _ => {
            syscall::print("PROC CONCURRENT WAIT=FAIL\r\n");
            return;
        }
    };
    let parent_won = parent_result == 0 && parent_status == 7;
    let parent_lost_clean = parent_result != 0 && parent_status == WAIT_SENTINEL;
    let exactly_one_won = parent_won ^ waiter_won;
    let loser_buffer_clean = if parent_won {
        waiter_lost_clean
    } else {
        parent_lost_clean
    };
    if exactly_one_won && loser_buffer_clean {
        syscall::print("PROC CONCURRENT WAIT=PASS\r\n");
    } else {
        syscall::print("PROC CONCURRENT WAIT=FAIL\r\n");
    }
}

pub fn concurrent_waiter(_argument: u64) -> ! {
    let parent = syscall::getppid();
    let mut request = ipc::Message::new(0, b"");
    if !syscall::receive(&mut request) || request.sender != parent {
        syscall::exit();
    }
    let mut bytes = [0_u8; 8];
    bytes.copy_from_slice(&request.payload[..8]);
    let target = u64::from_le_bytes(bytes);
    let mut status = WAIT_SENTINEL;
    let result = syscall::waitpid_raw(target, &mut status as *mut u64 as u64);
    let mut reply = ipc::Message::new(0, b"");
    reply.payload[0] = u8::from(result == 0 && status == 7);
    // Losing must leave the buffer as it was: ECHILD is not a status.
    reply.payload[1] = u8::from(result != 0 && status == WAIT_SENTINEL);
    let _ = syscall::send(parent, &reply);
    loop {
        syscall::sleep(1000);
    }
}

pub fn process_api_child() -> ! {
    let own = syscall::task_id();
    let ppid = syscall::getppid();
    if ppid != 0 && ppid != own {
        syscall::print("PROC CHILD PPID=PASS\r\n");
    } else {
        syscall::print("PROC CHILD PPID=FAIL\r\n");
    }
    syscall::exit_with_status(7)
}

/// Unprivileged mount probe: both mount and umount must be denied.
pub fn mount_denied_probe() -> ! {
    syscall::sleep(900);
    let dir = "/tmp/mnt-denied";
    let _ = syscall::fs_mkdir(dir);
    let mounted = syscall::fs_mount("/", dir, "ramfs", 0);
    let unmounted = syscall::fs_umount(dir, 0);
    let _ = syscall::fs_rmdir(dir);
    if mounted || unmounted {
        syscall::print("MOUNT DENY=FAIL\r\n");
    } else {
        syscall::print("MOUNT DENY=PASS\r\n");
    }
    loop {
        syscall::sleep(1000);
    }
}

/// Privileged mount probe (CAP_MOUNT provisioned by the demo harness):
/// mount, file creation inside the new backend, unmount.
pub fn mount_allowed_probe() -> ! {
    syscall::sleep(900);
    let dir = "/tmp/mnt-allowed";
    let file = "/tmp/mnt-allowed/file";
    let directory = syscall::fs_mkdir(dir);
    let mounted = directory && syscall::fs_mount("/", dir, "ramfs", 0);
    let created = mounted && syscall::fs_create(file);
    let removed = created && syscall::fs_unlink(file);
    let unmounted = mounted && syscall::fs_umount(dir, 0);
    let _ = syscall::fs_rmdir(dir);
    if mounted && created && removed && unmounted {
        syscall::print("MOUNT ALLOW=PASS\r\n");
    } else {
        syscall::print("MOUNT ALLOW=FAIL\r\n");
    }
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

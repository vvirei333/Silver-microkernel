use crate::{allocator, ipc, syscall};

const PRIVATE_WINDOW_VA: usize = 0x0000_0080_0000_0000;
const DEMO_USER_STACK_VA: usize = 0x7FE0_0000;
const DEMO_MMAP_SIZE: usize = 2 * 1024 * 1024;
const SECRET_PATTERN: [u8; 8] = [0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE];
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

    // T15-02: the request is all-or-nothing. Use a long-lived child so the
    // spawn handle's CAP_KILL cannot be retired by an early exit before these
    // grants run. The handle covers CAP_KILL and CAP_MMIO, but the parent owns
    // no self-CAP_MMIO, so the mixed delegation must be rejected as a whole.
    let own = syscall::task_id();
    if let Some(stable_cap) =
        syscall::spawn(syscall::PROGRAM_DEMO, crate::ROLE_SLEEP_OVERFLOW)
    {
        let mixed = syscall::cap_grant(stable_cap, own, syscall::CAP_KILL | syscall::CAP_MMIO);
        if !mixed && syscall::alloc_dma(4096).is_none() {
            syscall::print("SECURITY MIXED GRANT DENY=PASS\r\n");
        } else {
            syscall::print("SECURITY MIXED GRANT DENY=FAIL\r\n");
        }
        // Same handle, target-scoped bit only: delegation still works.
        if syscall::cap_grant(stable_cap, own, syscall::CAP_KILL) {
            syscall::print("SECURITY SCOPED GRANT=PASS\r\n");
        } else {
            syscall::print("SECURITY SCOPED GRANT=FAIL\r\n");
        }
        // Same handle, global bit only: still denied (handle is not ownership).
        if !syscall::cap_grant(stable_cap, own, syscall::CAP_MMIO) {
            syscall::print("SECURITY GLOBAL GRANT DENY=PASS\r\n");
        } else {
            syscall::print("SECURITY GLOBAL GRANT DENY=FAIL\r\n");
        }
        if let Some(stable) = syscall::cap_get_task_id(stable_cap) {
            let _ = syscall::kill(stable_cap);
            let _ = syscall::waitpid(stable);
        }
    } else {
        syscall::print("SECURITY MIXED GRANT DENY=FAIL\r\n");
        syscall::print("SECURITY SCOPED GRANT=FAIL\r\n");
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

fn write_secret(pointer: *mut u8, length: usize) {
    let mut offset = 0;
    while offset + SECRET_PATTERN.len() <= length {
        for (index, byte) in SECRET_PATTERN.iter().copied().enumerate() {
            unsafe { core::ptr::write_volatile(pointer.add(offset + index), byte) };
        }
        offset += 64;
    }
}

fn contains_secret(pointer: *const u8, length: usize) -> bool {
    if length < SECRET_PATTERN.len() {
        return false;
    }
    let mut offset = 0;
    while offset + SECRET_PATTERN.len() <= length {
        let mut matches = true;
        for (index, expected) in SECRET_PATTERN.iter().copied().enumerate() {
            if unsafe { core::ptr::read_volatile(pointer.add(offset + index)) } != expected {
                matches = false;
                break;
            }
        }
        if matches {
            return true;
        }
        offset += 1;
    }
    false
}

fn is_zero(pointer: *const u8, length: usize) -> bool {
    (0..length).all(|index| unsafe { core::ptr::read_volatile(pointer.add(index)) } == 0)
}

/// Writes a recognizable pattern into every user-visible surface, then exits.
/// The next task spawned by the supervisor should never see this pattern.
pub fn secret_writer() -> ! {
    write_secret(DEMO_USER_STACK_VA as *mut u8, 0x1000);
    write_secret(PRIVATE_WINDOW_VA as *mut u8, 4096);
    let Some(mapping) = syscall::mmap(0, DEMO_MMAP_SIZE, syscall::PROT_READ | syscall::PROT_WRITE)
    else {
        syscall::exit_with_status(1)
    };
    write_secret(mapping as *mut u8, DEMO_MMAP_SIZE);
    syscall::exit_with_status(7)
}

/// Freshly spawned after `secret_writer`: verifies zeroed stack/private/mmap
/// and that no writer pattern survived in recycled frames.
pub fn secret_reader() -> ! {
    let mut status = 0u32;
    if !is_zero(DEMO_USER_STACK_VA as *const u8, 64) {
        status |= 1 << 0;
    }
    if !is_zero(PRIVATE_WINDOW_VA as *const u8, 64) {
        status |= 1 << 1;
    }
    match syscall::mmap(0, DEMO_MMAP_SIZE, syscall::PROT_READ | syscall::PROT_WRITE) {
        Some(mapping) => {
            if !is_zero(mapping as *const u8, 64) {
                status |= 1 << 2;
            }
            if contains_secret(mapping as *const u8, DEMO_MMAP_SIZE) {
                status |= 1 << 5;
            }
        }
        None => status |= (1 << 2) | (1 << 5),
    }
    if contains_secret(DEMO_USER_STACK_VA as *const u8, 0x2000) {
        status |= 1 << 3;
    }
    if contains_secret(PRIVATE_WINDOW_VA as *const u8, 4096) {
        status |= 1 << 4;
    }
    syscall::exit_with_status(u64::from(status))
}

/// C-01 supervisor: runs writer/reader pairs so the reader reuses frames the
/// writer dirtied, then reports one aggregate result per surface.
pub fn memory_probe() -> ! {
    syscall::sleep(600);
    let mut failures = 0u32;
    let mut harness_ok = true;
    for _ in 0..3 {
        let Some(writer_cap) = syscall::spawn(syscall::PROGRAM_DEMO, crate::ROLE_SECRET_WRITER)
        else {
            harness_ok = false;
            break;
        };
        let Some(writer) = syscall::cap_get_task_id(writer_cap) else {
            harness_ok = false;
            break;
        };
        if syscall::waitpid(writer) != Some(7) {
            harness_ok = false;
        }

        let Some(reader_cap) = syscall::spawn(syscall::PROGRAM_DEMO, crate::ROLE_SECRET_READER)
        else {
            harness_ok = false;
            break;
        };
        let Some(reader) = syscall::cap_get_task_id(reader_cap) else {
            harness_ok = false;
            break;
        };
        match syscall::waitpid(reader) {
            Some(status) => failures |= status as u32,
            None => harness_ok = false,
        }
    }
    if !harness_ok {
        failures |= 0x3F;
    }
    if failures & (1 << 0) == 0 {
        syscall::print("MEM STACK ZERO=PASS\r\n");
    } else {
        syscall::print("MEM STACK ZERO=FAIL\r\n");
    }
    if failures & (1 << 1) == 0 {
        syscall::print("MEM PRIVATE ZERO=PASS\r\n");
    } else {
        syscall::print("MEM PRIVATE ZERO=FAIL\r\n");
    }
    if failures & (1 << 2) == 0 {
        syscall::print("MEM MMAP ZERO=PASS\r\n");
    } else {
        syscall::print("MEM MMAP ZERO=FAIL\r\n");
    }
    if failures & (1 << 3) == 0 {
        syscall::print("MEM SECRET STACK=PASS\r\n");
    } else {
        syscall::print("MEM SECRET STACK=FAIL\r\n");
    }
    if failures & (1 << 4) == 0 {
        syscall::print("MEM SECRET PRIVATE=PASS\r\n");
    } else {
        syscall::print("MEM SECRET PRIVATE=FAIL\r\n");
    }
    if failures & (1 << 5) == 0 {
        syscall::print("MEM SECRET MMAP=PASS\r\n");
    } else {
        syscall::print("MEM SECRET MMAP=FAIL\r\n");
    }
    loop {
        syscall::sleep(1000);
    }
}

/// Unprivileged power probe: `SYS_POWER_OFF` must be rejected before PSCI.
pub fn power_denied_probe() -> ! {
    syscall::sleep(700);
    if syscall::power_off_raw() != 0 {
        syscall::print("POWER DENY=PASS\r\n");
    } else {
        syscall::print("POWER DENY=FAIL\r\n");
    }
    loop {
        syscall::sleep(1000);
    }
}

/// Trusted power probe (CAP_POWER provisioned by the demo harness). A
/// successful PSCI SYSTEM_OFF does not return, so the attempt line is the
/// marker and QEMU's clean "Power down" exit is the pass condition.
pub fn power_trusted_probe() -> ! {
    syscall::sleep(30_000);
    syscall::print("POWER ALLOW=ATTEMPT\r\n");
    let _ = syscall::power_off_raw();
    syscall::print("POWER ALLOW=FAIL\r\n");
    syscall::exit_with_status(1)
}

/// H-02 probe: a file write must fail at the VFS file cap, not exhaust the
/// 2 MiB kernel heap. The file is removed afterwards.
pub fn resource_probe() -> ! {
    syscall::sleep(800);
    const CHUNK: usize = 1024;
    const EXPECTED_LIMIT: usize = 512 * 1024;
    let path = "/tmp/resource-limit";
    let _ = syscall::fs_unlink(path);
    if !syscall::fs_create(path) {
        syscall::print("RESOURCE FILE LIMIT=FAIL\r\n");
        loop {
            syscall::sleep(1000);
        }
    }
    let Some(fd) = syscall::fs_open(path, syscall::OPEN_WRITE) else {
        syscall::print("RESOURCE FILE LIMIT=FAIL\r\n");
        loop {
            syscall::sleep(1000);
        }
    };
    let chunk = [0x5A_u8; CHUNK];
    let mut written = 0usize;
    let mut limited = false;
    for _ in 0..600 {
        match syscall::fs_write(fd, &chunk) {
            Some(CHUNK) => written += CHUNK,
            Some(count) => written += count,
            None => {
                limited = true;
                break;
            }
        }
    }
    let _ = syscall::fs_close(fd);
    let _ = syscall::fs_unlink(path);
    if limited && written >= EXPECTED_LIMIT {
        syscall::print("RESOURCE FILE LIMIT=PASS\r\n");
    } else {
        syscall::print("RESOURCE FILE LIMIT=FAIL\r\n");
    }
    if vfs_budget_probe() {
        syscall::print("VFS BUDGET=PASS\r\n");
    } else {
        syscall::print("VFS BUDGET=FAIL\r\n");
    }
    loop {
        syscall::sleep(1000);
    }
}

const VFS_CHUNK: usize = 1024;

/// Writes exactly `total` bytes to a freshly truncated file. Returns false on
/// any short write, so a budget rejection also fails the setup phase.
fn write_file_exact(path: &str, total: usize) -> bool {
    let Some(fd) = syscall::fs_open(path, syscall::OPEN_WRITE) else {
        return false;
    };
    let chunk = [0x5A_u8; VFS_CHUNK];
    let mut written = 0;
    while written < total {
        let count = (total - written).min(VFS_CHUNK);
        match syscall::fs_write(fd, &chunk[..count]) {
            Some(done) if done > 0 => written += done,
            _ => {
                let _ = syscall::fs_close(fd);
                return false;
            }
        }
    }
    let _ = syscall::fs_close(fd);
    true
}

/// Continues a file from its current offset until the VFS rejects a write or
/// `limit` additional bytes were accepted. The fd offset is preserved, so the
/// caller can free budget and call this again.
fn append_until_failure(fd: u64, limit: usize) -> usize {
    let chunk = [0x6B_u8; VFS_CHUNK];
    let mut written = 0;
    while written < limit {
        let count = (limit - written).min(VFS_CHUNK);
        match syscall::fs_write(fd, &chunk[..count]) {
            Some(done) if done > 0 => written += done,
            _ => break,
        }
    }
    written
}

/// T17-03/T17-04: actual `Vec` capacity is charged to one system-wide budget,
/// not one budget per mount. A filler plus a full 512 KiB file leaves no room
/// for the second mount, but removing the first files lets the same fd finish
/// its 512 KiB file. That proves both the shared limit and that truncate /
/// unlink return capacity to the budget.
fn vfs_budget_probe() -> bool {
    // Filler and A are deliberately not the same logical and physical size:
    // A is 300 KiB logical but needs a 512 KiB `Vec` allocation. A length-only
    // accountant would let mount B reach its 512 KiB file cap; charging real
    // capacity leaves B short until A and the filler are removed.
    const FILLER_BYTES: usize = 64 * 1024;
    const FILE_A_BYTES: usize = 300 * 1024;
    const FILE_B_BYTES: usize = 512 * 1024;

    let filler = "/tmp/vfs-filler";
    let dir_a = "/tmp/vfs-budget-a";
    let file_a = "/tmp/vfs-budget-a/file";
    let dir_b = "/tmp/vfs-budget-b";
    let file_b = "/tmp/vfs-budget-b/file";

    let _ = syscall::fs_unlink(filler);
    let _ = syscall::fs_unlink(file_a);
    let _ = syscall::fs_unlink(file_b);
    let _ = syscall::fs_umount(dir_a, 0);
    let _ = syscall::fs_umount(dir_b, 0);
    let _ = syscall::fs_rmdir(dir_a);
    let _ = syscall::fs_rmdir(dir_b);

    let mut fd_b = None;
    let result = (|| -> bool {
        if !syscall::fs_create(filler) || !write_file_exact(filler, FILLER_BYTES) {
            return false;
        }
        if !syscall::fs_mkdir(dir_a) || !syscall::fs_mount("/", dir_a, "ramfs", 0) {
            return false;
        }
        if !syscall::fs_create(file_a) || !write_file_exact(file_a, FILE_A_BYTES) {
            return false;
        }
        if !syscall::fs_mkdir(dir_b) || !syscall::fs_mount("/", dir_b, "ramfs", 0) {
            return false;
        }
        if !syscall::fs_create(file_b) {
            return false;
        }
        let Some(fd) = syscall::fs_open(file_b, syscall::OPEN_WRITE) else {
            return false;
        };
        fd_b = Some(fd);
        // The second backend must hit the *shared* budget well before its own
        // 512 KiB file cap.
        let before = append_until_failure(fd, FILE_B_BYTES);
        if before >= FILE_B_BYTES {
            return false;
        }
        if !syscall::fs_unlink(filler) || !syscall::fs_unlink(file_a) {
            return false;
        }
        let after = append_until_failure(fd, FILE_B_BYTES - before);
        after == FILE_B_BYTES - before
    })();

    if let Some(fd) = fd_b {
        let _ = syscall::fs_close(fd);
    }
    let _ = syscall::fs_unlink(file_b);
    let _ = syscall::fs_umount(dir_b, 0);
    let _ = syscall::fs_umount(dir_a, 0);
    let _ = syscall::fs_rmdir(dir_b);
    let _ = syscall::fs_rmdir(dir_a);
    let _ = syscall::fs_unlink(filler);
    result
}

const DMA_TEST_BYTES: usize = 16 * 4096;

/// The parent grants CAP_MMIO just after `SYS_SPAWN`; a timer wakeup can let
/// the child run before that grant. Retry briefly so the DMA probe tests
/// zeroing rather than a scheduling race.
fn dma_alloc_with_retry() -> Option<(u64, u64)> {
    for _ in 0..100 {
        if let Some(buffer) = syscall::alloc_dma(DMA_TEST_BYTES as u64) {
            return Some(buffer);
        }
        syscall::sleep(10);
    }
    None
}

/// T17-02 writer: dirties every byte of a freshly allocated DMA run, then
/// exits so the frames go back to the allocator with the pattern still in
/// them.
pub fn dma_writer() -> ! {
    let Some((virtual_address, _)) = dma_alloc_with_retry() else {
        syscall::exit_with_status(1)
    };
    let pointer = virtual_address as *mut u8;
    write_secret(pointer, DMA_TEST_BYTES);
    if !contains_secret(pointer as *const u8, DMA_TEST_BYTES) {
        syscall::exit_with_status(2)
    }
    syscall::exit_with_status(0)
}

/// T17-02 reader: whatever DMA run the allocator returns must be zero. Used
/// after `dma_writer`, so a reused range would expose the pattern.
pub fn dma_reader() -> ! {
    let Some((virtual_address, _)) = dma_alloc_with_retry() else {
        syscall::exit_with_status(1)
    };
    let pointer = virtual_address as *const u8;
    if is_zero(pointer, DMA_TEST_BYTES) && !contains_secret(pointer, DMA_TEST_BYTES) {
        syscall::exit_with_status(0)
    } else {
        syscall::exit_with_status(3)
    }
}

fn run_dma_child(role: u64) -> Option<u64> {
    let cap = syscall::spawn(syscall::PROGRAM_DEMO, role)?;
    let id = syscall::cap_get_task_id(cap)?;
    if !syscall::cap_grant(cap, id, syscall::CAP_MMIO) {
        return None;
    }
    syscall::waitpid(id)
}

/// T17-02 and T17-06 probe. The harness grants CAP_MMIO:
/// * writer/reader pairs exercise recycled DMA frames (zero before mapping);
/// * then the CapTable is filled to the limit and `map_phys` must fail closed
///   instead of creating an unbookkept MMIO mapping.
pub fn mmio_dma_probe() -> ! {
    syscall::sleep(1400);

    let mut dma_failures = 0u32;
    for _ in 0..3 {
        if run_dma_child(crate::ROLE_DMA_WRITER) != Some(0) {
            dma_failures |= 1;
        }
        if run_dma_child(crate::ROLE_DMA_READER) != Some(0) {
            dma_failures |= 2;
        }
    }
    if dma_failures == 0 {
        syscall::print("DMA RECYCLED ZERO=PASS\r\n");
    } else {
        syscall::print("DMA RECYCLED ZERO=FAIL\r\n");
    }

    let own = syscall::task_id();
    let mut filled = 0u32;
    let mut mapping_denied = false;
    // A long-lived child keeps the spawn handle's CAP_ALL rights valid while
    // the parent fills its own table (an immediately exiting child could be
    // reaped first and the handle reduced to CAP_WAIT).
    if let Some(child_cap) = syscall::spawn(syscall::PROGRAM_DEMO, crate::ROLE_SLEEP_OVERFLOW) {
        for _ in 0..256 {
            if syscall::cap_grant(child_cap, own, syscall::CAP_KILL) {
                filled += 1;
            } else {
                break;
            }
        }
        // 0x0010_0000 is outside DRAM, so it is a valid MMIO target. With the
        // table full the syscall must return an error and create no mapping.
        mapping_denied = syscall::map_phys(0x0010_0000, 0x1000).is_none();
        if let Some(child) = syscall::cap_get_task_id(child_cap) {
            let _ = syscall::kill(child_cap);
            let _ = syscall::waitpid(child);
        }
    }
    syscall::print("MMIO FILLED=");
    syscall::print_dec_u64(filled as u64);
    syscall::print("\r\n");
    if filled > 0 && mapping_denied {
        syscall::print("MMIO CAPACITY DENY=PASS\r\n");
    } else {
        syscall::print("MMIO CAPACITY DENY=FAIL\r\n");
    }

    loop {
        syscall::sleep(1000);
    }
}

pub fn exit_immediate() -> ! {
    syscall::exit_with_status(0)
}

/// T17-05 probe: spawn many short-lived children without waiting. Zombie
/// records are capped independently of active-task slots, so spawning keeps
/// working; a final child can still be waited for and reports its status.
pub fn zombie_probe() -> ! {
    syscall::sleep(1200);
    const CHILDREN: usize = 70;
    let mut spawned = 0usize;
    for index in 0..CHILDREN {
        if syscall::spawn(syscall::PROGRAM_DEMO, crate::ROLE_EXIT_IMMEDIATE).is_none() {
            break;
        }
        spawned += 1;
        // Yield periodically so the children actually run, exit and become
        // zombies instead of filling the active-task limit.
        if index % 5 == 4 {
            syscall::sleep(10);
        }
    }
    syscall::sleep(200);

    let mut final_status = None;
    if let Some(cap) = syscall::spawn(syscall::PROGRAM_DEMO, crate::ROLE_EXIT_IMMEDIATE) {
        if let Some(child) = syscall::cap_get_task_id(cap) {
            final_status = syscall::waitpid(child);
        }
    }
    if spawned == CHILDREN {
        syscall::print("ZOMBIE SPAWN=PASS\r\n");
    } else {
        syscall::print("ZOMBIE SPAWN=FAIL\r\n");
    }
    if final_status == Some(0) {
        syscall::print("ZOMBIE WAIT=PASS\r\n");
    } else {
        syscall::print("ZOMBIE WAIT=FAIL\r\n");
    }
    loop {
        syscall::sleep(1000);
    }
}

/// T17-07: dynamic ELF ownership. A full parent CapTable makes `spawn_elf`
/// refuse *after* the image is loaded; the RAII release must return every
/// image/staging frame. The successful spawn/wait loop then checks that a
/// normal teardown also returns its frames (free-page count does not drop).
pub fn elf_lifecycle_probe() -> ! {
    // Run after the noisy boot probes settle; frame counts are only meaningful
    // when unrelated spawns are not in flight.
    syscall::sleep(8_000);

    let mut refusal_ok = false;
    if let Some(sleeper_cap) =
        syscall::spawn(syscall::PROGRAM_DEMO, crate::ROLE_SLEEP_OVERFLOW)
    {
        if let Some(sleeper) = syscall::cap_get_task_id(sleeper_cap) {
            let own = syscall::task_id();
            for _ in 0..256 {
                if !syscall::cap_grant(sleeper_cap, own, syscall::CAP_KILL) {
                    break;
                }
            }
            let free_before = syscall::memory_info().map(|info| info.free_bytes);
            let denied = syscall::spawn_path("/bin/hello").is_none();
            let free_after = syscall::memory_info().map(|info| info.free_bytes);
            refusal_ok = denied
                && match (free_before, free_after) {
                    (Some(before), Some(after)) => after >= before,
                    _ => false,
                };
            let _ = syscall::kill(sleeper_cap);
            let _ = syscall::waitpid(sleeper);
            // Give reap a chance to retire the CAP_KILL records so the table
            // has room for the successful spawns below.
            syscall::sleep(100);
        }
    }
    if refusal_ok {
        syscall::print("ELF REFUSAL RELEASE=PASS\r\n");
    } else {
        syscall::print("ELF REFUSAL RELEASE=FAIL\r\n");
    }

    let before = syscall::memory_info().map(|info| info.free_bytes);
    let mut launched = 0u32;
    for _ in 0..3 {
        let Some(cap) = syscall::spawn_path("/bin/hello") else {
            break;
        };
        let Some(child) = syscall::cap_get_task_id(cap) else {
            break;
        };
        // `hello` is a long-lived program; kill it so the dynamic image,
        // stacks and page tables go through the normal teardown/reap path.
        syscall::sleep(50);
        if !syscall::kill(cap) || syscall::waitpid(child).is_none() {
            break;
        }
        launched += 1;
    }
    syscall::sleep(100);
    let after = syscall::memory_info().map(|info| info.free_bytes);
    let frames_stable = match (before, after) {
        (Some(before), Some(after)) => after >= before,
        _ => false,
    };
    if launched == 3 && frames_stable {
        syscall::print("ELF REAP FRAMES=PASS\r\n");
    } else {
        syscall::print("ELF REAP FRAMES=FAIL\r\n");
    }

    loop {
        syscall::sleep(1000);
    }
}

/// Finds a capability slot that targets `task`. Demo-only: the harness
/// provisions the Scout handle without extending the syscall ABI.
fn find_cap_for_target(task: u64) -> Option<u64> {
    for id in 1..64 {
        if syscall::cap_get_task_id(id) == Some(task) {
            return Some(id);
        }
    }
    None
}

/// FIX 1: the nameserver probe validates a live round trip, then forces a
/// controlled Scout death while a lookup is blocked. A full mailbox keeps
/// Scout blocked in its reply, so the waiter is genuinely inside
/// `receive_from(Scout)` when the death notification arrives.
pub fn nameserver_probe() -> ! {
    // After every normal IPC/permission probe has finished using Scout.
    syscall::sleep(9_000);
    const SCOUT_ID: u64 = 1;

    let alive = syscall::register("ns-alive")
        && syscall::lookup("ns-alive") == Some(syscall::task_id())
        && syscall::unregister("ns-alive");
    if alive {
        syscall::print("NAMESERVER ALIVE=PASS\r\n");
    } else {
        syscall::print("NAMESERVER ALIVE=FAIL\r\n");
    }

    let mut death_ok = false;
    if let Some(scout_cap) = find_cap_for_target(SCOUT_ID) {
        if let Some(helper_cap) =
            syscall::spawn(syscall::PROGRAM_DEMO, crate::ROLE_NAMESERVER_FILLER)
        {
            if let Some(helper) = syscall::cap_get_task_id(helper_cap) {
                if syscall::cap_grant(scout_cap, helper, syscall::CAP_KILL) {
                    // The helper fills our 16-slot mailbox, then waits long
                    // enough for this lookup to block on Scout's reply.
                    syscall::sleep(300);
                    let reply = syscall::lookup("ns-during");
                    death_ok = reply.is_none();
                }
                let _ = syscall::waitpid(helper);
            }
        }
    }
    if death_ok {
        syscall::print("NAMESERVER WAIT DEATH=PASS\r\n");
    } else {
        syscall::print("NAMESERVER WAIT DEATH=FAIL\r\n");
    }

    syscall::sleep(50);
    let dead_ok = !syscall::task_alive(SCOUT_ID)
        && syscall::lookup("ns-after").is_none()
        && !syscall::register("ns-after");
    if dead_ok {
        syscall::print("NAMESERVER DEAD REQUEST=PASS\r\n");
    } else {
        syscall::print("NAMESERVER DEAD REQUEST=FAIL\r\n");
    }

    let mut repeat_ok = true;
    for _ in 0..3 {
        if syscall::lookup("ns-after").is_some() {
            repeat_ok = false;
        }
    }
    if repeat_ok {
        syscall::print("NAMESERVER REPEAT=PASS\r\n");
    } else {
        syscall::print("NAMESERVER REPEAT=FAIL\r\n");
    }

    loop {
        syscall::sleep(1000);
    }
}

/// Fills the parent's mailbox (blocking Scout's reply), then kills Scout via
/// the capability delegated by the probe.
pub fn nameserver_filler() -> ! {
    let parent = syscall::getppid();
    if parent == 0 {
        syscall::exit_with_status(1);
    }
    for _ in 0..16 {
        if !syscall::send(parent, &ipc::Message::new(syscall::task_id(), b"FILL")) {
            syscall::exit_with_status(2);
        }
    }
    syscall::sleep(1_200);
    let Some(scout_cap) = find_cap_for_target(1) else {
        syscall::exit_with_status(3)
    };
    if !syscall::kill(scout_cap) {
        syscall::exit_with_status(4);
    }
    syscall::exit_with_status(0)
}

/// FIX 2: W^X for anonymous mappings. RW/RX/R must work; RWX must fail before
/// any frame or `mmap_regions` entry is created, and the free-frame count must
/// not drop across the rejected request.
pub fn mmap_wx_probe() -> ! {
    syscall::sleep(5_000);
    const SIZE: usize = 2 * 4096;

    let probe = |prot: u64| -> bool {
        match syscall::mmap(0, SIZE, prot) {
            Some(address) => syscall::munmap(address, SIZE),
            None => false,
        }
    };
    let rw = probe(syscall::PROT_READ | syscall::PROT_WRITE);
    let rx = probe(syscall::PROT_READ | syscall::PROT_EXEC);
    let ro = probe(syscall::PROT_READ);

    let before = syscall::memory_info().map(|info| info.free_bytes);
    let rwx = syscall::mmap(
        0,
        SIZE,
        syscall::PROT_READ | syscall::PROT_WRITE | syscall::PROT_EXEC,
    );
    let after = syscall::memory_info().map(|info| info.free_bytes);
    let rwx_denied = rwx.is_none();
    let frames_stable = match (before, after) {
        (Some(before), Some(after)) => after >= before,
        _ => false,
    };

    if rw {
        syscall::print("MMAP RW=PASS\r\n");
    } else {
        syscall::print("MMAP RW=FAIL\r\n");
    }
    if rx {
        syscall::print("MMAP RX=PASS\r\n");
    } else {
        syscall::print("MMAP RX=FAIL\r\n");
    }
    if ro {
        syscall::print("MMAP R=PASS\r\n");
    } else {
        syscall::print("MMAP R=FAIL\r\n");
    }
    if rwx_denied {
        syscall::print("MMAP RWX DENY=PASS\r\n");
    } else {
        syscall::print("MMAP RWX DENY=FAIL\r\n");
    }
    if rwx_denied && frames_stable {
        syscall::print("MMAP RWX CLEAN=PASS\r\n");
    } else {
        syscall::print("MMAP RWX CLEAN=FAIL\r\n");
    }

    loop {
        syscall::sleep(1000);
    }
}

/// FIX 3: `SYS_FS_READ` must advance the offset only after a successful
/// copyout. The probe checks normal progress, EFAULT retry, EOF and zero-byte
/// reads on the same descriptor.
pub fn fs_read_probe() -> ! {
    const PATH: &str = "/tmp/fs-read-probe";
    const CONTENT: &[u8; 16] = b"ABCDEFGHIJKLMNOP";

    syscall::sleep(1_000);
    let _ = syscall::fs_unlink(PATH);
    if !syscall::fs_create(PATH) {
        syscall::print("FS_READ SETUP=FAIL\r\n");
        loop {
            syscall::sleep(1000);
        }
    }
    if let Some(fd) = syscall::fs_open(PATH, syscall::OPEN_WRITE) {
        let wrote = syscall::fs_write(fd, CONTENT).is_some();
        let _ = syscall::fs_close(fd);
        if !wrote {
            syscall::print("FS_READ SETUP=FAIL\r\n");
        }
    } else {
        syscall::print("FS_READ SETUP=FAIL\r\n");
    }

    // Zero-byte read must not advance the offset.
    let mut zero_ok = false;
    if let Some(fd) = syscall::fs_open(PATH, syscall::OPEN_READ) {
        let mut empty = [0_u8; 0];
        let mut first = [0_u8; 4];
        zero_ok = syscall::fs_read(fd, &mut empty) == Some(0)
            && syscall::fs_read(fd, &mut first) == Some(4)
            && &first == b"ABCD";
        let _ = syscall::fs_close(fd);
    }
    if zero_ok {
        syscall::print("FS_READ ZERO=PASS\r\n");
    } else {
        syscall::print("FS_READ ZERO=FAIL\r\n");
    }

    let mut offset_ok = false;
    let mut eof_ok = false;
    if let Some(fd) = syscall::fs_open(PATH, syscall::OPEN_READ) {
        let mut first = [0_u8; 4];
        let mut retry = [0_u8; 4];
        let read_first = syscall::fs_read(fd, &mut first) == Some(4) && &first == b"ABCD";
        // Null pointer: copy_to_user must fail and leave offset at 4.
        let faulted = syscall::fs_read_raw(fd, 0, 4).is_none();
        let retried = syscall::fs_read(fd, &mut retry) == Some(4) && &retry == b"EFGH";
        offset_ok = read_first && faulted && retried;

        let mut rest = [0_u8; 8];
        let read_rest = syscall::fs_read(fd, &mut rest) == Some(8) && &rest == b"IJKLMNOP";
        let eof = syscall::fs_read(fd, &mut retry) == Some(0);
        eof_ok = read_rest && eof;
        let _ = syscall::fs_close(fd);
    }
    if offset_ok {
        syscall::print("FS_READ OFFSET=PASS\r\n");
    } else {
        syscall::print("FS_READ OFFSET=FAIL\r\n");
    }
    if eof_ok {
        syscall::print("FS_READ EOF=PASS\r\n");
    } else {
        syscall::print("FS_READ EOF=FAIL\r\n");
    }

    let _ = syscall::fs_unlink(PATH);
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

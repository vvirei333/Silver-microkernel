//! Nameserver task: maps printable names to task IDs so tasks can find each
//! other without hardcoded IDs. It holds the well-known ID NAMESERVER_ID (1),
//! guaranteed by spawn order in scheduler::init.
//!
//! Protocol (mailbox messages, text payloads, request/reply over IPC):
//!   request "R <name>" — register the sender under <name>;
//!                        replies "OK" or "TAKEN" (no re-registration)
//!   request "L <name>" — look up <name>;
//!                        replies "ID <n>" or "MISSING"
//!
//! The table is fixed-capacity stack storage, not a Vec: user tasks have no
//! kernel heap mapping in the high-half design.

use crate::{ipc, syscall};

const MAX_ENTRIES: usize = 16;
const NAME_CAPACITY: usize = ipc::PAYLOAD_SIZE - 2;

#[derive(Copy, Clone)]
struct Entry {
    name: [u8; NAME_CAPACITY],
    name_len: u8,
    id: u64,
    used: bool,
}

impl Entry {
    const EMPTY: Self = Self {
        name: [0; NAME_CAPACITY],
        name_len: 0,
        id: 0,
        used: false,
    };

    fn name(&self) -> &[u8] {
        &self.name[..self.name_len as usize]
    }
}

/// Entry point spawned by the scheduler as an EL0 user task: it reaches the
/// kernel (IPC, console) only through system calls. Never returns.
pub extern "C" fn nameserver_task() -> ! {
    let mut table = [Entry::EMPTY; MAX_ENTRIES];
    syscall::print("NAMESERVER READY\r\n");

    loop {
        let mut request = ipc::Message::new(0, b"");
        syscall::receive(&mut request);
        let reply = match request.text().split_first() {
            Some((&b'R', name)) if !name.is_empty() => register(&mut table, name, request.sender),
            Some((&b'L', name)) if !name.is_empty() => lookup(&table, name),
            _ => text_reply("WHAT?"),
        };
        syscall::send(request.sender, &reply);
    }
}

fn register(table: &mut [Entry], name: &[u8], id: u64) -> ipc::Message {
    if name.len() > NAME_CAPACITY {
        return text_reply("TAKEN");
    }
    if table.iter().any(|entry| entry.used && entry.name() == name) {
        return text_reply("TAKEN");
    }

    let Some(entry) = table.iter_mut().find(|entry| !entry.used) else {
        return text_reply("FULL");
    };
    entry.name[..name.len()].copy_from_slice(name);
    entry.name_len = name.len() as u8;
    entry.id = id;
    entry.used = true;

    syscall::print("NAMESERVER REG ");
    syscall::print(core::str::from_utf8(name).unwrap_or("?"));
    syscall::print("\r\n");
    text_reply("OK")
}

fn lookup(table: &[Entry], name: &[u8]) -> ipc::Message {
    match table
        .iter()
        .find(|entry| entry.used && entry.name() == name)
    {
        Some(entry) => id_reply(entry.id),
        None => text_reply("MISSING"),
    }
}

fn text_reply(text: &str) -> ipc::Message {
    ipc::Message::new(syscall::task_id(), text.as_bytes())
}

/// Formats an "ID <n>" reply without alloc::format — kernel style.
fn id_reply(id: u64) -> ipc::Message {
    let mut payload = [0_u8; ipc::PAYLOAD_SIZE];
    payload[0] = b'I';
    payload[1] = b'D';
    payload[2] = b' ';

    let mut digits = [0_u8; 20];
    let mut count = 0;
    let mut rest = id;
    while rest > 0 {
        digits[count] = b'0' + (rest % 10) as u8;
        rest /= 10;
        count += 1;
    }
    for index in 0..count {
        payload[3 + index] = digits[count - 1 - index];
    }

    ipc::Message {
        sender: syscall::task_id(),
        payload,
    }
}

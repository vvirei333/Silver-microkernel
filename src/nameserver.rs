//! Nameserver task: maps printable names to task IDs so tasks can find each
//! other without hardcoded IDs. It holds the well-known ID NAMESERVER_ID (1),
//! guaranteed by spawn order in scheduler::init — spawn it before anything
//! that wants to register or look up a name.
//!
//! Protocol (mailbox messages, text payloads, request/reply over IPC):
//!   request "R <name>" — register the sender under <name>;
//!                        replies "OK" or "TAKEN" (no re-registration)
//!   request "L <name>" — look up <name>;
//!                        replies "ID <n>" or "MISSING"

use alloc::vec::Vec;

use crate::{ipc, syscall};

struct Entry {
    name: Vec<u8>,
    id: u64,
}

/// Entry point spawned by the scheduler as an EL0 user task: it reaches the
/// kernel (IPC, console) only through system calls. Never returns.
pub extern "C" fn nameserver_task() -> ! {
    let mut table: Vec<Entry> = Vec::new();
    syscall::print("NAMESERVER READY\r\n");

    loop {
        let mut request = ipc::Message::new(0, b"");
        syscall::receive(&mut request);
        let reply = match request.text().split_first() {
            Some((&b'R', name)) if !name.is_empty() => register(&mut table, name, request.sender),
            Some((&b'L', name)) if !name.is_empty() => lookup(&mut table, name),
            _ => text_reply("WHAT?"),
        };
        syscall::send(request.sender, &reply);
    }
}

fn register(table: &mut Vec<Entry>, name: &[u8], id: u64) -> ipc::Message {
    if table.iter().any(|entry| entry.name == name) {
        return text_reply("TAKEN");
    }

    table.push(Entry {
        name: name.to_vec(),
        id,
    });
    syscall::print("NAMESERVER REG ");
    syscall::print(core::str::from_utf8(name).unwrap_or("?"));
    syscall::print("\r\n");
    text_reply("OK")
}

fn lookup(table: &[Entry], name: &[u8]) -> ipc::Message {
    match table.iter().find(|entry| entry.name == name) {
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

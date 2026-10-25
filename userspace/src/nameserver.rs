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

pub fn run() -> ! {
    let mut table = [Entry::EMPTY; MAX_ENTRIES];
    syscall::print("SCOUT READY\r\n");

    loop {
        let mut request = ipc::Message::new(0, b"");
        syscall::receive(&mut request);
        purge_dead(&mut table);
        let reply = match request.text().split_first() {
            Some((&b'R', name)) if !name.is_empty() => register(&mut table, name, request.sender),
            Some((&b'U', name)) if !name.is_empty() => unregister(&mut table, name, request.sender),
            Some((&b'L', name)) if !name.is_empty() => lookup(&table, name),
            _ => text_reply(b"WHAT?"),
        };
        syscall::send(request.sender, &reply);
    }
}

fn register(table: &mut [Entry], name: &[u8], id: u64) -> ipc::Message {
    if name.len() > NAME_CAPACITY || table.iter().any(|entry| entry.used && entry.name() == name) {
        return text_reply(b"TAKEN");
    }
    let Some(entry) = table.iter_mut().find(|entry| !entry.used) else {
        return text_reply(b"FULL");
    };
    entry.name[..name.len()].copy_from_slice(name);
    entry.name_len = name.len() as u8;
    entry.id = id;
    entry.used = true;

    syscall::print("SCOUT REG ");
    syscall::print(core::str::from_utf8(name).unwrap_or("?"));
    syscall::print("\r\n");
    text_reply(b"OK")
}

fn unregister(table: &mut [Entry], name: &[u8], owner: u64) -> ipc::Message {
    let Some(entry) = table
        .iter_mut()
        .find(|entry| entry.used && entry.name() == name && entry.id == owner)
    else {
        return text_reply(b"MISSING");
    };
    entry.used = false;
    syscall::print("SCOUT UNREG ");
    syscall::print(core::str::from_utf8(name).unwrap_or("?"));
    syscall::print("\r\n");
    text_reply(b"OK")
}

fn purge_dead(table: &mut [Entry]) {
    for entry in table.iter_mut().filter(|entry| entry.used) {
        if !syscall::task_alive(entry.id) {
            entry.used = false;
        }
    }
}

fn lookup(table: &[Entry], name: &[u8]) -> ipc::Message {
    match table
        .iter()
        .find(|entry| entry.used && entry.name() == name)
    {
        Some(entry) => id_reply(entry.id),
        None => text_reply(b"MISSING"),
    }
}

fn text_reply(text: &[u8]) -> ipc::Message {
    ipc::Message::new(syscall::task_id(), text)
}

fn id_reply(id: u64) -> ipc::Message {
    let mut payload = [0_u8; ipc::PAYLOAD_SIZE];
    payload[..3].copy_from_slice(b"ID ");
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

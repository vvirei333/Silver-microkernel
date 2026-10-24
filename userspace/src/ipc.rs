pub const PAYLOAD_SIZE: usize = 32;

#[derive(Copy, Clone)]
#[repr(C)]
pub struct Message {
    pub sender: u64,
    pub payload: [u8; PAYLOAD_SIZE],
}

impl Message {
    pub fn new(sender: u64, bytes: &[u8]) -> Self {
        let mut payload = [0_u8; PAYLOAD_SIZE];
        let len = bytes.len().min(PAYLOAD_SIZE);
        payload[..len].copy_from_slice(&bytes[..len]);
        Self { sender, payload }
    }

    pub fn text(&self) -> &[u8] {
        let end = self
            .payload
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(PAYLOAD_SIZE);
        &self.payload[..end]
    }
}

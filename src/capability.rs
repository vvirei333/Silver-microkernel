use alloc::vec::Vec;

pub type CapId = u64;

pub const CAP_KILL: u32 = 1 << 0;
pub const CAP_WAIT: u32 = 1 << 1;
pub const CAP_CONSOLE_GRANT: u32 = 1 << 2;
pub const CAP_RESUME: u32 = 1 << 3;
pub const CAP_MMIO: u32 = 1 << 4;
pub const CAP_IRQ: u32 = 1 << 5;
pub const CAP_ALL: u32 = CAP_KILL | CAP_WAIT | CAP_CONSOLE_GRANT | CAP_RESUME | CAP_MMIO | CAP_IRQ;

pub struct Cap {
    pub id: CapId,
    pub target_task: u64,
    pub rights: u32,
}

pub struct CapTable {
    caps: Vec<Cap>,
    next_id: CapId,
}

impl CapTable {
    pub fn new() -> Self {
        Self {
            caps: Vec::new(),
            next_id: 1,
        }
    }

    pub fn insert(&mut self, target_task: u64, rights: u32) -> CapId {
        let id = self.next_id;
        self.next_id += 1;
        self.caps.push(Cap {
            id,
            target_task,
            rights,
        });
        id
    }

    pub fn get(&self, id: CapId) -> Option<&Cap> {
        self.caps.iter().find(|cap| cap.id == id)
    }

    pub fn grant(&self, cap_id: CapId, rights_mask: u32) -> Option<(u64, u32)> {
        let cap = self.get(cap_id)?;
        if cap.rights & rights_mask != rights_mask {
            return None;
        }
        Some((cap.target_task, rights_mask))
    }

    pub fn has_rights(&self, rights_mask: u32) -> bool {
        self.caps
            .iter()
            .any(|cap| cap.rights & rights_mask == rights_mask)
    }

    pub fn get_mut(&mut self, id: CapId) -> Option<&mut Cap> {
        self.caps.iter_mut().find(|cap| cap.id == id)
    }

    pub fn revoke(&mut self, id: CapId) {
        self.caps.retain(|cap| cap.id != id);
    }

    pub fn remove_for_target(&mut self, target_task: u64) {
        self.caps.retain(|cap| cap.target_task != target_task);
    }

    pub fn retire_for_target(&mut self, target_task: u64) {
        self.caps.retain_mut(|cap| {
            if cap.target_task != target_task {
                true
            } else if cap.rights & CAP_WAIT != 0 {
                cap.rights = CAP_WAIT;
                true
            } else {
                false
            }
        });
    }

    pub fn remove_all(&mut self) {
        self.caps.clear();
    }
}

use alloc::vec::Vec;

pub type CapId = u64;

pub const CAP_KILL: u32 = 1 << 0;
pub const CAP_WAIT: u32 = 1 << 1;
pub const CAP_CONSOLE_GRANT: u32 = 1 << 2;
pub const CAP_RESUME: u32 = 1 << 3;
pub const CAP_MMIO: u32 = 1 << 4;
pub const CAP_IRQ: u32 = 1 << 5;
pub const CAP_PCI: u32 = 1 << 6;
/// Global system privilege: change the kernel mount table (`SYS_MOUNT` /
/// `SYS_UMOUNT`). Deliberately kept out of [`CAP_ALL`]: spawn handles carry
/// `CAP_ALL`, and a handle over a child must never become an own privilege.
pub const CAP_MOUNT: u32 = 1 << 7;
/// Global system privilege: power the machine off via PSCI `SYSTEM_OFF`.
/// Deliberately kept out of [`CAP_ALL`]; only trusted components (SILVERD,
/// the graphics driver and explicitly provisioned probes) hold it.
pub const CAP_POWER: u32 = 1 << 8;

/// Rights that describe authority over a specific `target_task`. They are
/// only meaningful together with a target and are checked with
/// [`CapTable::has_target_rights`] / [`CapTable::grant`].
pub const CAP_TARGET_BITS: u32 = CAP_KILL | CAP_WAIT | CAP_CONSOLE_GRANT | CAP_RESUME;

/// Rights that describe a process-wide/system privilege of the holder. Such a
/// right is held only by a self-capability (`target_task == own id`), never by
/// a handle on another task.
pub const CAP_GLOBAL_BITS: u32 = CAP_MMIO | CAP_IRQ | CAP_PCI | CAP_MOUNT | CAP_POWER;

pub const CAP_ALL: u32 =
    CAP_KILL | CAP_WAIT | CAP_CONSOLE_GRANT | CAP_RESUME | CAP_MMIO | CAP_IRQ | CAP_PCI;

/// Upper bound on capability records in one task. Spawn handles alone need up
/// to `MAX_TASKS` entries in a parent; repeated `SYS_CAP_GRANT` must not grow
/// the table without limit.
pub const MAX_CAPS_PER_TASK: usize = 96;

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

    /// True while the table can accept another record. Callers that can be
    /// driven by an EL0 loop (`SYS_CAP_GRANT`) must check this first.
    pub fn has_capacity(&self) -> bool {
        self.caps.len() < MAX_CAPS_PER_TASK
    }

    /// True when `count` more records fit. A mixed global|scoped grant adds
    /// two records, so a single-slot check is not enough.
    pub fn has_capacity_for(&self, count: usize) -> bool {
        self.caps.len().saturating_add(count) <= MAX_CAPS_PER_TASK
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

    /// Own privileges of `owner`: only capabilities whose target is the task
    /// itself count. A spawn handle (`target_task == child`) or a delegated
    /// handle on some other task is authority *over that target*, not a
    /// process-wide privilege of the holder.
    pub fn has_self_rights(&self, owner: u64, rights_mask: u32) -> bool {
        self.caps.iter().any(|cap| {
            cap.target_task == owner && cap.rights & rights_mask == rights_mask
        })
    }

    /// True when a capability for `target_task` carries at least `rights_mask`.
    /// Used by `SYS_WAITPID` to authorize a non-child target without exposing
    /// arbitrary PID access.
    pub fn has_target_rights(&self, target_task: u64, rights_mask: u32) -> bool {
        self.caps.iter().any(|cap| {
            cap.target_task == target_task && cap.rights & rights_mask == rights_mask
        })
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

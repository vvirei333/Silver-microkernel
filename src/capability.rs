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
/// Write access to the filesystem (create/truncate/write/unlink/mkdir/rmdir/
/// rename). Apps spawned from `/sfs/apps` do not hold it: an installed ELF can
/// read the tree but cannot modify the disk until a bundle manifest grants it.
pub const CAP_FS_WRITE: u32 = 1 << 9;
/// Authority to map a specific physical range of another task's DMA window.
/// `SYS_DMA_GRANT_SELF` inserts it with the owner's id and `(base, size)`;
/// a generic `SYS_CAP_GRANT` cannot delegate it.
pub const CAP_DMA_SHARE: u32 = 1 << 10;

/// Rights that describe authority over a specific `target_task`. They are
/// only meaningful together with a target and are checked with
/// [`CapTable::has_target_rights`] / [`CapTable::grant`].
pub const CAP_TARGET_BITS: u32 =
    CAP_KILL | CAP_WAIT | CAP_CONSOLE_GRANT | CAP_RESUME | CAP_DMA_SHARE;

/// Rights that describe a process-wide/system privilege of the holder. Such a
/// right is held only by a self-capability (`target_task == own id`), never by
/// a handle on another task.
pub const CAP_GLOBAL_BITS: u32 =
    CAP_MMIO | CAP_IRQ | CAP_PCI | CAP_MOUNT | CAP_POWER | CAP_FS_WRITE;

pub const CAP_ALL: u32 = CAP_KILL
    | CAP_WAIT
    | CAP_CONSOLE_GRANT
    | CAP_RESUME
    | CAP_MMIO
    | CAP_IRQ
    | CAP_PCI
    | CAP_FS_WRITE
    | CAP_DMA_SHARE;

/// Upper bound on capability records in one task. Spawn handles alone need up
/// to `MAX_TASKS` entries in a parent; repeated `SYS_CAP_GRANT` must not grow
/// the table without limit.
pub const MAX_CAPS_PER_TASK: usize = 96;

pub struct Cap {
    pub id: CapId,
    pub target_task: u64,
    pub rights: u32,
    /// Physical range this capability is confined to. Only `CAP_DMA_SHARE`
    /// uses it today: a share grant names `[base, base + size)` of the
    /// target's DMA window, and `map_phys` accepts only ranges inside it.
    /// `None` means the right is not range-scoped (and, for DMA share, does
    /// not authorize a foreign mapping at all).
    pub region: Option<(u64, u64)>,
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
        self.insert_region(target_task, rights, None)
    }

    /// Inserts a capability confined to a physical range. Used by
    /// `SYS_DMA_GRANT_SELF`: the owner of a DMA run hands a recipient the
    /// right to map exactly that run (or a sub-range).
    pub fn insert_region(
        &mut self,
        target_task: u64,
        rights: u32,
        region: Option<(u64, u64)>,
    ) -> CapId {
        let id = self.next_id;
        self.next_id += 1;
        self.caps.push(Cap {
            id,
            target_task,
            rights,
            region,
        });
        id
    }

    /// True while the table can accept another record. Callers that can be
    /// driven by an EL0 loop (`SYS_CAP_GRANT`) must check this first.
    pub fn has_capacity(&self) -> bool {
        self.caps.len() < MAX_CAPS_PER_TASK
    }

    pub fn len(&self) -> usize {
        self.caps.len()
    }

    /// True when `count` more records fit. A mixed global|scoped grant adds
    /// two records, so a single-slot check is not enough.
    pub fn has_capacity_for(&self, count: usize) -> bool {
        self.caps.len().saturating_add(count) <= MAX_CAPS_PER_TASK
    }

    pub fn get(&self, id: CapId) -> Option<&Cap> {
        self.caps.iter().find(|cap| cap.id == id)
    }

    pub fn grant(
        &self,
        cap_id: CapId,
        rights_mask: u32,
    ) -> Option<(u64, u32, Option<(u64, u64)>)> {
        let cap = self.get(cap_id)?;
        if cap.rights & rights_mask != rights_mask {
            return None;
        }
        Some((cap.target_task, rights_mask, cap.region))
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

    /// True when a region-scoped `CAP_DMA_SHARE` for `target_task` covers the
    /// whole physical range `[base, base + size)`. A capability without a
    /// region (a spawn handle, or a legacy owner-wide grant) never authorizes
    /// a foreign mapping: DMA sharing is region-scoped by construction.
    pub fn has_dma_share(&self, target_task: u64, base: u64, size: u64) -> bool {
        let Some(end) = base.checked_add(size) else {
            return false;
        };
        self.caps.iter().any(|cap| {
            if cap.target_task != target_task || cap.rights & CAP_DMA_SHARE == 0 {
                return false;
            }
            let Some((region_base, region_size)) = cap.region else {
                return false;
            };
            let Some(region_end) = region_base.checked_add(region_size) else {
                return false;
            };
            base >= region_base && end <= region_end
        })
    }

    /// First capability for `target_task` covering `rights_mask`. Used by
    /// `SYS_CAP_FIND` so a task can delegate a scoped right it received.
    pub fn find_for_target(&self, target_task: u64, rights_mask: u32) -> Option<CapId> {
        self.caps
            .iter()
            .find(|cap| cap.target_task == target_task && cap.rights & rights_mask == rights_mask)
            .map(|cap| cap.id)
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

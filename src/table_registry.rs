//! Ownership registry for physical frames used as page tables.
//!
//! The frame allocator tags every allocation with a generation, but a raw
//! address cannot prove *which* allocation a caller owns: once a table frame is
//! freed and recycled as another table, an old caller that only remembers the
//! address looks identical to the current owner.
//!
//! This registry closes that gap with [`TableToken`]s. A token is produced
//! exactly once, when the table frame is registered, and carries the allocator
//! generation of that allocation. Retiring a table requires presenting a token
//! whose generation still matches the registry record. A stale token is
//! refused and the frame is leaked rather than freed — a leak cannot hand live
//! page-table memory to a second owner.
//!
//! Every record also names the address space that owns the table (`owner`,
//! the physical root frame / TTBR0 value). Teardown drains one space's records
//! by root token; a walk-based free retires a frame only when it belongs to the
//! space being walked.
//!
//! The type is pure `Vec` logic so the token rules can be regression-tested on
//! the host; the kernel wraps it in a spinlock (`mmu::PAGE_TABLE_FRAMES`).

use alloc::vec::Vec;

/// A registered page-table frame plus the generation of its allocation.
///
/// Tokens are issued by [`TableRegistry::register`]. There is deliberately no
/// public constructor: constructing a token manually does not establish
/// ownership, and only the registry can produce the exact `(frame, generation)`
/// pair that [`TableRegistry::retire`] accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TableToken {
    frame: usize,
    generation: u32,
}

impl TableToken {
    const fn new(frame: usize, generation: u32) -> Self {
        Self { frame, generation }
    }

    pub const fn frame(self) -> usize {
        self.frame
    }

    pub const fn generation(self) -> u32 {
        self.generation
    }

    /// A placeholder that can never match a registered record: `register`
    /// refuses frame 0 and generation 0, and a live allocation never carries
    /// generation 0.
    pub const fn empty() -> Self {
        Self::new(0, 0)
    }
}

/// Outcome of retiring one table record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetireResult {
    /// Record removed; the caller may free `generation`'s allocation.
    Retired { generation: u32 },
    /// No record for the frame. Either never registered or already retired.
    NotRegistered,
    /// A record exists but belongs to a newer allocation of the same frame.
    StaleToken,
    /// A record exists for a different address space than the walker's.
    WrongOwner,
}

/// Outcome of retiring a whole address space by its root token.
#[derive(Debug, PartialEq, Eq)]
pub enum SpaceRetire {
    /// Every record owned by the space, removed in one atomic step.
    Retired(Vec<TableToken>),
    NotRegistered,
    StaleToken,
    /// The record exists but is not registered as a space root (its owner is
    /// another space), so draining it would free foreign tables.
    NotSpaceRoot,
    /// The result vector could not be reserved before any record was removed;
    /// the registry is unchanged.
    OutOfMemory,
}

struct TableRecord {
    token: TableToken,
    owner: usize,
}

/// Registry of live page-table frames. Pure logic; callers provide locking.
pub struct TableRegistry {
    records: Vec<TableRecord>,
}

impl TableRegistry {
    pub const fn new() -> Self {
        Self {
            records: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Registers a freshly allocated table frame. Returns `None` for a
    /// duplicate frame, a frame/generation that cannot belong to a live page
    /// table (frame 0 or generation 0), or when the record vector cannot grow.
    /// In every refusal case the registry is untouched and the caller still
    /// owns the raw allocation.
    pub fn register(
        &mut self,
        frame: usize,
        generation: u32,
        owner: usize,
    ) -> Option<TableToken> {
        // Frame 0 and generation 0 are reserved sentinels: the allocator never
        // issues them, and `TableToken::empty()` relies on that.
        if frame == 0 || generation == 0 {
            return None;
        }
        if self.records.iter().any(|record| record.token.frame == frame) {
            return None;
        }
        self.records.try_reserve(1).ok()?;
        let token = TableToken::new(frame, generation);
        self.records.push(TableRecord { token, owner });
        Some(token)
    }

    pub fn contains(&self, frame: usize) -> bool {
        self.owner(frame).is_some()
    }

    /// Address-space root that owns the live registration of `frame`, or
    /// `None` when the frame is not registered. Read-only: unlike
    /// [`Self::retire_owned`], this never changes the registry, so a table
    /// walker can validate a whole chain before its first mutation.
    pub fn owner(&self, frame: usize) -> Option<usize> {
        self.records
            .iter()
            .find(|record| record.token.frame == frame)
            .map(|record| record.owner)
    }

    /// Current token for `frame`, for tests only. Production code must keep
    /// the token it received at registration: asking for the current one
    /// would defeat the stale-token check.
    #[cfg(test)]
    pub fn token(&self, frame: usize) -> Option<TableToken> {
        self.records
            .iter()
            .find(|record| record.token.frame == frame)
            .map(|record| record.token)
    }

    /// Retires `token` only when the registry still holds that exact
    /// generation for the frame. This is the stale-token check.
    pub fn retire(&mut self, token: TableToken) -> RetireResult {
        let Some(position) = self
            .records
            .iter()
            .position(|record| record.token.frame == token.frame)
        else {
            return RetireResult::NotRegistered;
        };
        if self.records[position].token.generation != token.generation {
            return RetireResult::StaleToken;
        }
        let record = self.records.swap_remove(position);
        RetireResult::Retired {
            generation: record.token.generation,
        }
    }

    /// Retires a frame reached by walking `owner`'s page tables. The owner
    /// check keeps a recycled frame registered by another space from being
    /// freed through a stale descriptor in this one.
    pub fn retire_owned(&mut self, frame: usize, owner: usize) -> RetireResult {
        let Some(position) = self
            .records
            .iter()
            .position(|record| record.token.frame == frame)
        else {
            return RetireResult::NotRegistered;
        };
        if self.records[position].owner != owner {
            return RetireResult::WrongOwner;
        }
        let record = self.records.swap_remove(position);
        RetireResult::Retired {
            generation: record.token.generation,
        }
    }

    /// Drains every record owned by the space whose root token is `root`.
    ///
    /// Fails closed: a missing root, a stale root token, a root record that is
    /// not itself a space root, or a failed result allocation leaves the
    /// registry completely untouched.
    pub fn retire_space(&mut self, root: TableToken) -> SpaceRetire {
        self.retire_space_with(root, |retired, count| {
            retired.try_reserve(count).is_ok()
        })
    }

    /// `retire_space` with the result reservation injected, so the refusal
    /// path can be exercised on the host without a real allocator failure.
    fn retire_space_with<F>(&mut self, root: TableToken, reserve: F) -> SpaceRetire
    where
        F: FnOnce(&mut Vec<TableToken>, usize) -> bool,
    {
        let Some(position) = self
            .records
            .iter()
            .position(|record| record.token.frame == root.frame)
        else {
            return SpaceRetire::NotRegistered;
        };
        if self.records[position].token.generation != root.generation {
            return SpaceRetire::StaleToken;
        }
        if self.records[position].owner != root.frame {
            return SpaceRetire::NotSpaceRoot;
        }

        // The root record has `owner == root.frame`, so it is part of this
        // count; no separate "root will be collected" check is needed.
        let count = self
            .records
            .iter()
            .filter(|record| record.owner == root.frame)
            .count();
        let mut retired = Vec::new();
        if !reserve(&mut retired, count) {
            return SpaceRetire::OutOfMemory;
        }

        // Every fallible step is done: this loop only moves already-reserved
        // tokens out of the registry.
        let mut index = 0;
        while index < self.records.len() {
            if self.records[index].owner == root.frame {
                let record = self.records.swap_remove(index);
                retired.push(record.token);
            } else {
                index += 1;
            }
        }
        debug_assert!(retired.iter().any(|token| *token == root));
        SpaceRetire::Retired(retired)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        RetireResult, SpaceRetire, TableRegistry, TableToken,
    };

    const ROOT: usize = 0x4000_0000;
    const OTHER_ROOT: usize = 0x4000_1000;
    const FRAME: usize = 0x4001_0000;
    const FRAME_B: usize = 0x4001_1000;

    #[test]
    fn registration_returns_a_token_and_refuses_duplicates() {
        let mut registry = TableRegistry::new();
        let token = registry.register(FRAME, 7, ROOT).expect("first registration");
        assert_eq!(token, TableToken::new(FRAME, 7));
        assert_eq!(registry.len(), 1);

        assert_eq!(registry.register(FRAME, 8, ROOT), None);
        assert_eq!(registry.len(), 1);
        assert_eq!(registry.token(FRAME), Some(token));
    }

    #[test]
    fn owner_query_reports_the_registered_space() {
        let mut registry = TableRegistry::new();
        assert_eq!(registry.owner(FRAME), None);

        let token = registry.register(FRAME, 7, ROOT).expect("registration");
        assert_eq!(registry.owner(FRAME), Some(ROOT));
        assert_eq!(registry.owner(FRAME_B), None);

        // A space root is registered as its own owner.
        registry
            .register(OTHER_ROOT, 8, OTHER_ROOT)
            .expect("root registration");
        assert_eq!(registry.owner(OTHER_ROOT), Some(OTHER_ROOT));

        assert_eq!(
            registry.retire(token),
            RetireResult::Retired { generation: 7 }
        );
        assert_eq!(registry.owner(FRAME), None);

        // A recycled frame reports its new owner, so an old walk of ROOT can
        // no longer mistake it for its own table.
        registry
            .register(FRAME, 9, OTHER_ROOT)
            .expect("re-registration");
        assert_eq!(registry.owner(FRAME), Some(OTHER_ROOT));
    }

    #[test]
    fn stale_token_is_refused_after_the_frame_is_recycled() {
        let mut registry = TableRegistry::new();
        let old = registry.register(FRAME, 7, ROOT).expect("old registration");
        assert_eq!(
            registry.retire(old),
            RetireResult::Retired { generation: 7 }
        );

        // Frame recycled as a new table with a new generation.
        let new = registry
            .register(FRAME, 8, OTHER_ROOT)
            .expect("re-registration");
        assert_ne!(old, new);

        // The stale owner still remembers the old token.
        assert_eq!(registry.retire(old), RetireResult::StaleToken);
        // Fail closed: the live record must be untouched.
        assert!(registry.contains(FRAME));
        assert_eq!(
            registry.retire(new),
            RetireResult::Retired { generation: 8 }
        );
    }

    #[test]
    fn double_retire_is_refused() {
        let mut registry = TableRegistry::new();
        let token = registry.register(FRAME, 3, ROOT).expect("registration");
        assert_eq!(
            registry.retire(token),
            RetireResult::Retired { generation: 3 }
        );
        assert_eq!(registry.retire(token), RetireResult::NotRegistered);
        assert!(registry.is_empty());
    }

    #[test]
    fn walk_free_refuses_a_frame_owned_by_another_space() {
        let mut registry = TableRegistry::new();
        let token = registry
            .register(FRAME, 5, OTHER_ROOT)
            .expect("registration");
        assert_eq!(registry.retire_owned(FRAME, ROOT), RetireResult::WrongOwner);
        assert!(registry.contains(FRAME));
        assert_eq!(
            registry.retire_owned(FRAME, OTHER_ROOT),
            RetireResult::Retired { generation: 5 }
        );
        assert_eq!(registry.retire(token), RetireResult::NotRegistered);
    }

    #[test]
    fn space_retire_drains_only_that_space() {
        let mut registry = TableRegistry::new();
        let root = registry.register(ROOT, 1, ROOT).expect("root");
        let child = registry.register(FRAME, 2, ROOT).expect("child");
        let child_b = registry.register(FRAME_B, 3, ROOT).expect("child b");
        let foreign = registry
            .register(OTHER_ROOT, 4, OTHER_ROOT)
            .expect("foreign root");

        let SpaceRetire::Retired(mut drained) = registry.retire_space(root) else {
            panic!("space should retire");
        };
        drained.sort_by_key(|token| token.frame());
        let mut expected = [root, child, child_b];
        expected.sort_by_key(|token| token.frame());
        assert_eq!(drained, expected.to_vec());

        // The foreign space is untouched.
        assert!(registry.contains(OTHER_ROOT));
        assert_eq!(registry.len(), 1);
        assert_eq!(
            registry.retire(foreign),
            RetireResult::Retired { generation: 4 }
        );
    }

    #[test]
    fn space_retire_with_a_stale_root_token_changes_nothing() {
        let mut registry = TableRegistry::new();
        let old_root = registry.register(ROOT, 1, ROOT).expect("old root");
        assert!(matches!(
            registry.retire_space(old_root),
            SpaceRetire::Retired(_)
        ));
        let new_root = registry.register(ROOT, 2, ROOT).expect("new root");
        registry.register(FRAME, 3, ROOT).expect("child");

        assert_eq!(registry.retire_space(old_root), SpaceRetire::StaleToken);
        assert_eq!(registry.len(), 2);
        assert!(matches!(
            registry.retire_space(new_root),
            SpaceRetire::Retired(_)
        ));
        assert!(registry.is_empty());
    }

    #[test]
    fn space_retire_refuses_a_root_token_that_is_not_a_space_root() {
        let mut registry = TableRegistry::new();
        registry.register(ROOT, 1, OTHER_ROOT).expect("child registration");
        let child = registry.token(ROOT).expect("token");
        assert_eq!(registry.retire_space(child), SpaceRetire::NotSpaceRoot);
        assert!(registry.contains(ROOT));
    }

    #[test]
    fn register_rejects_zero_frame_and_zero_generation() {
        let mut registry = TableRegistry::new();
        assert_eq!(registry.register(0, 5, ROOT), None);
        assert_eq!(registry.register(FRAME, 0, ROOT), None);
        assert!(registry.is_empty());
    }

    #[test]
    fn space_retire_reservation_failure_changes_nothing() {
        let mut registry = TableRegistry::new();
        let root = registry.register(ROOT, 1, ROOT).expect("root");
        registry.register(FRAME, 2, ROOT).expect("child");
        registry.register(OTHER_ROOT, 3, OTHER_ROOT).expect("foreign");

        assert_eq!(
            registry.retire_space_with(root, |_, _| false),
            SpaceRetire::OutOfMemory
        );
        // No record may have been removed before the reservation succeeded.
        assert_eq!(registry.len(), 3);
        assert!(registry.contains(ROOT));
        assert!(registry.contains(FRAME));
        assert!(registry.contains(OTHER_ROOT));

        // The real path still drains exactly this space.
        assert!(matches!(registry.retire_space(root), SpaceRetire::Retired(_)));
        assert_eq!(registry.len(), 1);
        assert!(registry.contains(OTHER_ROOT));
    }
}

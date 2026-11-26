//! Task data types and the per-task address-space/ledger methods.
//!
//! Split out of `scheduler/mod.rs` (`REFACTOR.md` step 2). The type graph is
//! the stable part of the scheduler: `Task` and everything it embeds move
//! together, and `impl Task` — the `brk`/`mmap`/region bookkeeping that used
//! to live at the bottom of the file — moves with them.
//!
//! Visibility: fields and inherent methods that `scheduler/mod.rs` reads are
//! `pub(super)`, i.e. visible inside the scheduler module and nowhere else.
//! That is the minimum a data move needs; nothing becomes `pub(crate)`.

use super::*;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TaskState {
    Ready,
    Running,
    Sleeping(u64),
    Blocked,
    BlockedUntil(u64),
    Stopped,
    Dead,
    /// Terminated, executable resources already released; the small record
    /// survives only until a permitted wait consumes its exit status.
    Zombie,
}

#[repr(C, align(16))]
pub struct TaskContext {
    pub(super) registers: [u64; 12],
    pub(super) stack_pointer: u64,
    pub(super) user_stack_pointer: u64,
    pub(super) elr: u64,
    /// User TLS (TPIDR_EL0). Kernel per-CPU state lives in TPIDR_EL1.
    pub(super) tls: u64,
}

impl TaskContext {
    pub(super) const fn zero() -> Self {
        Self {
            registers: [0; 12],
            stack_pointer: 0,
            user_stack_pointer: 0,
            elr: 0,
            tls: 0,
        }
    }
}

#[derive(Copy, Clone)]
#[repr(C)]
pub struct TaskInfo {
    pub id: u64,
    pub state: u64,
    pub program: u64,
    pub argument: u64,
}

/// One anonymous mapping installed by `SYS_MMAP`. Kept sorted by
/// Ownership handle for a true `MAP_SHARED` memory-file window. The frames
/// belong to the memfd, so unmapping this region must not free them; holding
/// the handle keeps the memfd (and its frames) alive while a mapping exists.
#[derive(Clone, Default)]
pub(super) struct SharedMapping {
    #[cfg(feature = "linux-abi")]
    pub(super) owner: Option<crate::linux_fd::MappedFile>,
}

impl SharedMapping {
    pub(super) fn is_some(&self) -> bool {
        #[cfg(feature = "linux-abi")]
        {
            self.owner.is_some()
        }
        #[cfg(not(feature = "linux-abi"))]
        {
            false
        }
    }
}

/// `virtual_address` so the first-fit allocator can scan in a single pass.
#[derive(Clone)]
pub(super) struct MmapRegion {
    pub(super) virtual_address: usize,
    pub(super) page_count: usize,
    pub(super) physical_start: usize,
    pub(super) generation: u32,
    /// `PROT_*` the mapping was created with; `fork` must reuse it.
    pub(super) prot: u64,
    /// Set for a `MAP_SHARED` memfd window: the frames are memfd-owned.
    pub(super) shared: SharedMapping,
    /// File-backed `MAP_PRIVATE` pages are allocated one frame at a time (a
    /// library segment is far larger than the contiguous allocator can still
    /// find after a few hundred mappings). Empty for anonymous and shared
    /// regions, whose frames are one contiguous run in `physical_start`.
    pub(super) frames: Vec<(usize, u32)>,
    /// Anonymous reservation whose frames are allocated on first touch.
    /// Linux hands out large `PROT_NONE` reservations (glibc's 128 MiB
    /// `new_heap` probe) expecting no memory to be committed; an eager
    /// allocator answers `ENOMEM` and glibc retries forever.
    pub(super) lazy: bool,
    /// Faulted pages of a lazy region, keyed by page offset. Sparse, because
    /// a 128 MiB reservation usually has only a few MiB touched.
    pub(super) pages: BTreeMap<u32, (usize, u32)>,
    /// File-backed lazy region: `(path, byte offset of the mapping inside the
    /// file)`. Pages are read from the file on first touch, so mapping a
    /// 160 MiB library costs only the pages the program actually executes.
    /// The SFS image is immutable, so a path is a stable reference even after
    /// the client closes the descriptor; a file removed from a writable mount
    /// turns the fault into a task kill instead of a stale read.
    pub(super) file: Option<(alloc::string::String, usize)>,
}

impl MmapRegion {
    /// Frees the faulted pages with offsets in `[start, start + count)` and
    /// returns how many frames were released.
    pub(super) fn free_pages_range(&mut self, start: u32, count: u32) -> usize {
        let end = start.saturating_add(count);
        let keys: Vec<u32> = self.pages.range(start..end).map(|(&key, _)| key).collect();
        let mut freed = 0;
        for key in keys {
            if let Some((page, generation)) = self.pages.remove(&key) {
                let _ = frame_allocator::free_page_tagged(page, generation);
                freed += 1;
            }
        }
        freed
    }

    /// Splits off the pages at or above `at` into a new map keyed from zero.
    pub(super) fn take_pages_from(&mut self, at: u32) -> BTreeMap<u32, (usize, u32)> {
        let keys: Vec<u32> = self.pages.range(at..).map(|(&key, _)| key).collect();
        let mut tail = BTreeMap::new();
        for key in keys {
            if let Some(value) = self.pages.remove(&key) {
                tail.insert(key - at, value);
            }
        }
        tail
    }
}

/// Syscall ABI a task is running on.
///
/// `Native` is the Silver ABI (`x8` = `SYS_*`). `Linux` is the AArch64 Linux
/// ABI: `x8` carries a Linux syscall number and the kernel translates it. The
/// personality is fixed when the task is spawned from its ELF image; it is not
/// switchable by the task itself.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Personality {
    Native,
    #[cfg(feature = "linux-abi")]
    Linux,
}

/// Upper bound on a task's anonymous mappings. The virtual window allows far
/// more entries than the 128 MiB DRAM can back; this cap bounds the metadata
/// Vec itself, not just the frames. Denial's Flutter/Mesa process fragments
/// its window past 4096 entries during startup (fontconfig + GL caches), so
/// leave headroom while keeping the metadata bounded to a few MiB.
pub(super) const MAX_MMAP_REGIONS: usize = 16384;

/// Per-task cap on mapping bytes. The mmap window is ~1 GiB but DRAM is
/// shared by every task, so one EL0 task must not be able to claim the whole
/// machine and starve the scheduler, IPC and the kernel heap. The GTK3
/// library set is ~80 MiB of segments; 160 MiB leaves room for its heap while
/// the global frame reserve stays the last line of defence.
// A large value because lazily reserved ranges (glibc's 128 MiB heap probe)
// count toward it while committing no frames. The real bound on memory is the
// per-task frame ledger, charged one page at a time. Mesa/LLVM reserves
// ~193 MiB of sparse JIT/Gallium arenas per GPU context (compositor +
// transfer worker), which needs more than the historical 1 GiB now that the
// user VA window reaches 0xF0000000.
pub(super) const MAX_MMAP_BYTES_PER_TASK: usize = 2 * 1024 * 1024 * 1024;

/// How a task left the system: a normal exit code or a terminating signal.
///
/// The native wait ABI exposes only `code`; the Linux ABI converts a signal
/// into the low seven bits of a wait status so `WIFSIGNALED`/`WTERMSIG` work.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ExitStatus {
    pub code: u64,
    pub signal: Option<u8>,
}

impl ExitStatus {
    pub fn exited(code: u64) -> Self {
        Self { code, signal: None }
    }
}

pub struct Task {
    pub id: u64,
    pub state: TaskState,
    pub(super) resume_state: Option<TaskState>,
    pub program: u64,
    pub argument: u64,
    /// Syscall ABI this task entered userspace on.
    pub(super) personality: Personality,
    /// Capsicum-style capability mode. Once set, path-based syscalls are
    /// refused; only already-open descriptors and their rights remain.
    pub(super) cap_mode: bool,
    pub cap_table: capability::CapTable,
    /// Path prefix this task may write under, taken from the bundle manifest.
    /// Only meaningful when the task has no global `CAP_FS_WRITE`.
    pub(super) fs_write_prefix: Option<String>,
    /// Read-only subtree a Linux task may resolve. `None` means unrestricted
    /// (native tasks); Linux tasks default to `/sfs`.
    pub(super) read_prefix: Option<String>,
    /// Optional `(block limit, inode limit)` for the write prefix. `None`
    /// means no per-bundle quota; trusted tasks are never limited.
    pub(super) fs_quota: Option<(u32, u32)>,
    pub bound_irq: Option<u32>,
    pub(super) irq_pending: bool,
    pub(super) arguments: Vec<u8>,
    /// Parent PID recorded at spawn. Reparented to the live reaper when the
    /// parent dies, so an orphan is never left pointing at a dead PID.
    pub(super) ppid: Option<u64>,
    /// Exit status written by exit/kill and consumed exactly once by wait.
    pub(super) exit_status: Option<ExitStatus>,
    /// Set when the executable resources (stacks, image, address space, DMA,
    /// heap, cwd, argv/envp) have been released. A `Zombie` only keeps this
    /// metadata; `wait` removes the record.
    pub(super) resources_released: bool,
    /// Versioned argv/envp delivered on the initial user stack.
    pub(super) argv: Vec<Vec<u8>>,
    pub(super) envp: Vec<Vec<u8>>,
    pub(super) cwd: String,
    pub(super) owns_console: bool,
    /// True only for per-CPU idle tasks. Kept explicit because AP idle tasks
    /// are spawned after the boot tasks to preserve SCOUT_ID and the other
    /// spawn-order identities.
    pub(super) is_idle: bool,
    /// Dispatch class (0 high, 1 normal, 2 low). Kernel-only: userspace tasks
    /// are always Normal, and aging lives in the ready queue, not here.
    pub(super) priority: u8,
    pub context: TaskContext,
    /// Saved vector register file (q0-q31, FPCR, FPSR). The exception
    /// trampolines keep the live copy in `PerCpu::fp` while this task runs;
    /// `schedule` swaps it here on a context switch.
    pub fp: smp::FpState,
    pub stack: MemoryRegion,
    pub user_stack: MemoryRegion,
    pub ttbr0: usize,
    pub(super) user_image: Option<LoadedImage>,
    /// A foreign (Linux ABI) image, mapped at its own virtual addresses.
    pub(super) linux_image: Option<LinuxImage>,
    /// `(physical page, allocation generation)` for every `brk` page. The
    /// tag makes a stale teardown free fail instead of releasing a frame that
    /// was already recycled.
    pub(super) heap_pages: Vec<(usize, u32)>,
    pub(super) user_stack_generation: u32,
    pub(super) private_page: usize,
    pub(super) private_page_generation: u32,
    /// Root token for `ttbr0`: the only proof that teardown still owns this
    /// address space. A stale token is refused by the page-table registry.
    pub(super) space_token: TableToken,
    /// False for a `CLONE_VM` thread: the address space belongs to the leader
    /// and must survive this task's exit.
    pub(super) owns_space: bool,
    /// Thread-group id. `getpid` returns it; `gettid` returns `id`.
    pub(super) tgid: u64,
    /// Linux PID 1 view: an init process must report `getpid() == 1`, or
    /// BusyBox init refuses to run. Set only for a task spawned from a
    /// program named `init`; its children report `1` as their parent.
    pub(super) pid_one: bool,
    pub(super) is_thread: bool,
    /// `CLONE_CHILD_CLEARTID`: clear this word and futex-wake it on exit.
    pub(super) clear_child_tid: Option<u64>,
    pub(super) dma_pages: Vec<(usize, usize, u32)>,
    pub(super) mmap_regions: Vec<MmapRegion>,
    /// Sum of `mmap_regions` spans, bounded by [`MAX_MMAP_BYTES_PER_TASK`].
    /// Kept separately so the quota check is O(1), not a walk per request.
    pub(super) mmap_bytes: usize,
    /// Cached RX page with `mov x8, #139; svc #0`, used as the return address
    /// when a signal disposition has no SA_RESTORER. It lives in
    /// `mmap_regions`, so ordinary teardown frees it.
    pub(super) signal_trampoline: Option<usize>,
    /// Frames this task has charged to its ledger: every frame it can pin,
    /// including the fixed address-space tables, stacks, image, heap, mmap and
    /// DMA. Bounded by `mem_policy::task_frame_limit`; the global user-frame
    /// reserve is the second, machine-wide bound.
    pub(super) mem_frames: usize,
    pub(super) program_break: usize,
    pub(super) waiting_on: Option<WaitLink>,
    pub(super) owned_mutexes: Vec<*mut mutex::MutexState>,
    /// CPU currently executing this task; `Some` exactly while the task is
    /// in `Running` state. Guarded by the kernel lock.
    pub(super) running_on: Option<usize>,
}

impl Task {
    pub(super) fn new(
        id: u64,
        entry: Option<extern "C" fn() -> !>,
        user: Option<(u64, u64)>,
        image: Option<LoadedImage>,
        linux_image: Option<LinuxImage>,
        arguments: Vec<u8>,
        argv: Vec<Vec<u8>>,
        envp: Vec<Vec<u8>>,
        ppid: Option<u64>,
        fs_write_prefix: Option<String>,
        fs_quota: Option<(u32, u32)>,
        personality: Personality,
    ) -> Option<Self> {
        // Build the argv/envp stack image before allocating anything: a
        // malformed or oversized argument list must fail without leaking.
        let mut argv_block = if argv.is_empty() && envp.is_empty() {
            None
        } else {
            if user.is_none() {
                return None;
            }
            // Linux wants the same argc/argv/envp prefix plus auxv. The values
            // are derivable from the loaded image, so a foreign ELF starts with
            // a stack its libc understands.
            #[cfg(feature = "linux-abi")]
            let linux_auxv: Vec<(u64, u64)> = match personality {
                #[cfg(feature = "linux-abi")]
                Personality::Linux => linux_image
                    .as_ref()
                    .map(|image| linux_auxv(AuxvImage::linux(image)))
                    .or_else(|| image.as_ref().map(|image| linux_auxv(AuxvImage::native(image))))
                    .unwrap_or_default(),
                Personality::Native => Vec::new(),
            };
            Some(match personality {
                Personality::Native => silver_microkernel::build_argv_envp(
                    &argv,
                    &envp,
                    mmu::USER_STACK_VA + mmu::USER_STACK_SIZE,
                    mmu::USER_STACK_SIZE - 1024,
                )?,
                #[cfg(feature = "linux-abi")]
                Personality::Linux => silver_microkernel::build_linux_stack(
                    &argv,
                    &envp,
                    &linux_auxv,
                    mmu::USER_STACK_VA + mmu::USER_STACK_SIZE,
                    mmu::USER_STACK_SIZE - 1024,
                )?,
            })
        };
        // `build_linux_stack` reserves the first 16 bytes for AT_RANDOM. The
        // host-test builder is deterministic; the real kernel replaces those
        // bytes with CSPRNG output here. Native stacks have no such prefix.
        #[cfg(feature = "linux-abi")]
        if personality == Personality::Linux {
            if let Some(block) = argv_block.as_mut() {
                if block.block.len() >= 16 {
                    crate::entropy::fill(&mut block.block[..16]);
                }
            }
        }
        if argv_block.is_none() && !(argv.is_empty() && envp.is_empty()) {
            crate::console::write_str("TASK: argv/envp stack failed\r\n");
            return None;
        }
        // Charge the fixed footprint before allocating anything: the image
        // pages already exist (RAII releases them if this returns None), and
        // every other frame is allocated below. Page tables beyond the fixed
        // space are not charged separately (at most one L3 per 2 MiB).
        #[cfg(feature = "linux-abi")]
        let image_frames = image.as_ref().map_or(0, |image| image.page_count)
            + linux_image.as_ref().map_or(0, |image| image.page_count());
        #[cfg(not(feature = "linux-abi"))]
        let image_frames = image.as_ref().map_or(0, |image| image.page_count);
        let ledger_frames = TASK_STACK_PAGES
            + mmu::SPACE_TABLE_FRAMES
            + 1 // private page
            + if user.is_some() {
                TASK_USER_STACK_PAGES + image_frames
            } else {
                0
            };
        if !mem_policy::task_charge_ok(0, ledger_frames, task_frame_limit()) {
            crate::console::write_str("TASK: frame ledger refused\r\n");
            return None;
        }

        let Some(stack_start) = kernel_stack::alloc() else {
            crate::console::write_str("TASK: kernel stack alloc failed\r\n");
            return None;
        };

        let (user_stack, user_stack_generation) = if user.is_some() {
            let Some((user_start, generation)) =
                frame_allocator::alloc_user_contiguous_pages_tagged(TASK_USER_STACK_PAGES)
            else {
                kernel_stack::free(stack_start);
                return None;
            };
            (
                MemoryRegion::new(user_start, user_start + TASK_USER_STACK_SIZE),
                generation,
            )
        } else {
            (MemoryRegion::new(0, 0), 0)
        };

        let Some((private_page, private_page_generation)) = frame_allocator::alloc_user_page_tagged()
        else {
            if !user_stack.is_empty() {
                frame_allocator::free_contiguous_pages_tagged(
                    user_stack.start,
                    TASK_USER_STACK_PAGES,
                    user_stack_generation,
                );
            }
            kernel_stack::free(stack_start);
            return None;
        };

        let user_image = image;

        let Some((ttbr0, space_token)) =
            mmu::create_task_space(private_page, user_stack.start, user_image.as_ref(), false)
        else {
            if let Some(image) = &user_image {
                image.release();
            }
            #[cfg(feature = "linux-abi")]
            if let Some(image) = &linux_image {
                image.release();
            }
            frame_allocator::free_page_tagged(private_page, private_page_generation);
            if !user_stack.is_empty() {
                frame_allocator::free_contiguous_pages_tagged(
                    user_stack.start,
                    TASK_USER_STACK_PAGES,
                    user_stack_generation,
                );
            }
            kernel_stack::free(stack_start);
            return None;
        };

        // A Linux image is mapped at its own p_vaddr after the base space
        // exists; the loader cannot do it earlier because the space root is
        // created here. `destroy_task_space` also frees the private page.
        #[cfg(feature = "linux-abi")]
        if let Some(image) = &linux_image {
            if !mmu::install_linux_image(ttbr0, image) {
                crate::console::write_str("TASK: install_linux_image failed\r\n");
                mmu::destroy_task_space(
                    space_token,
                    private_page,
                    private_page_generation,
                    mmu::asid_of(ttbr0),
                );
                if let Some(image) = &user_image {
                    image.release();
                }
                image.release();
                if !user_stack.is_empty() {
                    frame_allocator::free_contiguous_pages_tagged(
                        user_stack.start,
                        TASK_USER_STACK_PAGES,
                        user_stack_generation,
                    );
                }
                kernel_stack::free(stack_start);
                return None;
            }
        }

        // Publish the argv/envp block on the freshly mapped user stack before
        // the task can run. The stack VA range is backed by `user_stack`, so
        // the physical page behind a stack VA is `user_stack.start + offset`.
        let user_stack_pointer = if let Some(layout) = &argv_block {
            let offset = mmu::USER_STACK_SIZE - layout.block.len();
            let destination = phys_to_virt(user_stack.start + offset) as *mut u8;
            unsafe {
                core::ptr::copy_nonoverlapping(
                    layout.block.as_ptr(),
                    destination,
                    layout.block.len(),
                );
            }
            layout.sp as u64
        } else if user.is_some() {
            (mmu::USER_STACK_VA + mmu::USER_STACK_SIZE) as u64
        } else {
            0
        };

        let stack = MemoryRegion::new(stack_start, stack_start + TASK_STACK_SIZE);
        let mut context = TaskContext::zero();
        #[cfg(feature = "linux-abi")]
        let linux_entry_va = linux_image.as_ref().map(|image| image.entry as u64);
        #[cfg(not(feature = "linux-abi"))]
        let linux_entry_va: Option<u64> = None;
        context.registers[0] = if let Some(image) = &user_image {
            image.entry as u64
        } else if let Some(linux_entry) = linux_entry_va {
            linux_entry
        } else {
            entry.expect("kernel task entry is required") as usize as u64
        };
        context.registers[1] = user.map_or(0, |(_, argument)| argument);
        context.elr = if user.is_some() {
            silver_user_bootstrap as *const () as usize as u64
        } else {
            entry.expect("kernel task entry is required") as usize as u64
        };
        context.stack_pointer = phys_to_virt(stack.end) as u64;
        context.user_stack_pointer = user_stack_pointer;

        let mut cap_table = capability::CapTable::new();
        if let Some((program, _)) = user {
            if let Some(rights) = initial_self_rights(program) {
                cap_table.insert(id, rights);
            }
            // Device MMIO is not ambient: each built-in driver gets exactly
            // the range the DT/platform assigned to it, as a self capability
            // with a region. `map_phys` requires this for non-DMA addresses.
            if let Some(range) = crate::driver_resources::mmio_range(program) {
                cap_table.insert_region(
                    id,
                    capability::CAP_MMIO,
                    Some((range.base, range.size)),
                );
            }
            // The IRQ grant is node-scoped too: `(intid, 1)`. The QEMU UART
            // gets 33; a DT-bound UART gets whatever its `interrupts` says.
            if let Some(intid) = crate::driver_resources::irq(program) {
                cap_table.insert_region(
                    id,
                    capability::CAP_IRQ,
                    Some((u64::from(intid), 1)),
                );
            }
        }

        #[cfg(feature = "linux-abi")]
        let pid_one = personality == Personality::Linux
            && argv
                .first()
                .map(|argument| {
                    let name = argument.rsplit(|byte| *byte == b'/').next().unwrap_or(argument);
                    name == b"init"
                })
                .unwrap_or(false);
        #[cfg(not(feature = "linux-abi"))]
        let pid_one = false;
        #[cfg(feature = "linux-abi")]
        let read_prefix = match personality {
            Personality::Linux => Some(String::from("/sfs")),
            Personality::Native => None,
        };
        #[cfg(not(feature = "linux-abi"))]
        let read_prefix: Option<String> = {
            let _ = personality;
            None
        };
        Some(Self {
            fs_write_prefix,
            fs_quota,
            read_prefix,
            owns_space: true,
            tgid: id,
            pid_one,
            is_thread: false,
            clear_child_tid: None,
            id,
            state: TaskState::Ready,
            resume_state: None,
            is_idle: false,
            priority: sched_policy::PRIORITY_NORMAL,
            program: user.map_or(u64::MAX, |(program, _)| program),
            argument: user.map_or(0, |(_, argument)| argument),
            personality,
            cap_mode: false,
            cap_table,
            bound_irq: None,
            irq_pending: false,
            arguments,
            ppid,
            exit_status: None,
            resources_released: false,
            argv,
            envp,
            cwd: match personality {
                #[cfg(feature = "linux-abi")]
                Personality::Linux => String::from("/sfs"),
                Personality::Native => String::from("/"),
            },
            owns_console: cfg!(feature = "demo-tasks"),
            context,
            fp: smp::FpState::zero(),
            stack,
            user_stack,
            user_stack_generation,
            private_page,
            private_page_generation,
            space_token,
            ttbr0,
            user_image,
            linux_image,
            heap_pages: Vec::new(),
            dma_pages: Vec::new(),
            mmap_regions: Vec::new(),
            mmap_bytes: 0,
            signal_trampoline: None,
            mem_frames: ledger_frames,
            program_break: mmu::USER_HEAP_VA,
            waiting_on: None,
            owned_mutexes: Vec::new(),
            running_on: None,
        })
    }

    /// Charges `count` frames to this task's ledger. All-or-nothing: the
    /// counter is untouched on refusal.
    pub(super) fn charge_frames(&mut self, count: usize) -> bool {
        if !mem_policy::task_charge_ok(self.mem_frames, count, task_frame_limit()) {
            return false;
        }
        // `task_charge_ok` already proved `mem_frames + count` cannot overflow.
        self.mem_frames += count;
        true
    }

    pub(super) fn release_frames(&mut self, count: usize) {
        self.mem_frames = self.mem_frames.saturating_sub(count);
    }

    pub(super) fn change_program_break(&mut self, requested: usize) -> Option<usize> {
        if requested == 0 {
            return Some(self.program_break);
        }
        if (self.user_image.is_none() && self.linux_image.is_none())
            || requested < mmu::USER_HEAP_VA
            || requested > mmu::USER_HEAP_VA + mmu::USER_HEAP_MAX_SIZE
        {
            return None;
        }

        let target_pages = (requested - mmu::USER_HEAP_VA).div_ceil(PAGE_SIZE);
        let original_pages = self.heap_pages.len();
        if target_pages > original_pages {
            // Allocate the whole growth first and publish it as one batch:
            // hundreds of pages used to pay a full table clean plus
            // `dsb`/`tlbi` each. On any failure every freshly allocated frame
            // is returned and the address space is untouched.
            let growth = target_pages - original_pages;
            if !self.charge_frames(growth) {
                return None;
            }
            let mut frames: Vec<usize> = Vec::with_capacity(growth);
            let mut generations: Vec<u32> = Vec::with_capacity(growth);
            for _ in 0..growth {
                let Some((frame, generation)) = frame_allocator::alloc_user_page_tagged() else {
                    for (&frame, &generation) in frames.iter().zip(generations.iter()) {
                        frame_allocator::free_page_tagged(frame, generation);
                    }
                    self.release_frames(growth);
                    return None;
                };
                frames.push(frame);
                generations.push(generation);
            }
            if !mmu::map_user_heap_pages(self.ttbr0, original_pages, &frames) {
                for (&frame, &generation) in frames.iter().zip(generations.iter()) {
                    frame_allocator::free_page_tagged(frame, generation);
                }
                self.release_frames(growth);
                return None;
            }
            self.heap_pages
                .extend(frames.into_iter().zip(generations.into_iter()));
        }

        self.truncate_heap(target_pages);
        self.program_break = requested;
        Some(requested)
    }

    pub(super) fn truncate_heap(&mut self, target_pages: usize) {
        let before = self.heap_pages.len();
        while self.heap_pages.len() > target_pages {
            let index = self.heap_pages.len() - 1;
            let mapped = mmu::unmap_user_heap_page(self.ttbr0, index)
                .expect("tracked user heap mapping is missing");
            let tracked = self.heap_pages.pop().unwrap();
            debug_assert_eq!(mapped, tracked.0);
            frame_allocator::free_page_tagged(mapped, tracked.1);
        }
        self.release_frames(before.saturating_sub(self.heap_pages.len()));
    }

    /// Validates a mapping request and reserves its virtual span, frame
    /// budget and bookkeeping slot. Shared by the contiguous (anonymous /
    /// memfd) path and the per-page file path.
    pub(super) fn mmap_prepare(
        &mut self,
        length: usize,
        prot: u64,
        fixed: Option<usize>,
        lazy: bool,
    ) -> Option<(usize, usize, usize)> {
        // W^X: a single mapping must never be writable and executable at the
        // same time. Validate before placement, frame allocation, PTE
        // publication or `mmap_regions` bookkeeping, so a rejected request
        // leaves no state behind.
        if prot & mmu::PROT_WRITE != 0 && prot & mmu::PROT_EXEC != 0 {
            return None;
        }

        let (virtual_address, page_count) = match fixed {
            Some(address) => {
                let page_count = length.div_ceil(PAGE_SIZE);
                if page_count == 0 || page_count > mmu::USER_MMAP_MAX_PAGES {
                    return None;
                }
                if address % PAGE_SIZE != 0 || address < mmu::USER_MMAP_VA {
                    return None;
                }
                let span = page_count.checked_mul(PAGE_SIZE)?;
                let end = address.checked_add(span)?;
                if end > mmu::USER_MMAP_MAX_VA {
                    return None;
                }
                // Linux `MAP_FIXED` replaces whatever was mapped in the range.
                // The dynamic loader relies on this after reserving a whole
                // library span with `PROT_NONE` and mapping each segment over
                // it. Unmapping first also keeps the region list sorted.
                if !self.munmap_range(address, span) {
                    return None;
                }
                (address, page_count)
            }
            None => silver_microkernel::mmap_place(
                length,
                mmu::USER_MMAP_VA,
                mmu::USER_MMAP_MAX_VA,
                PAGE_SIZE,
                self.mmap_regions
                    .iter()
                    .map(|region| (region.virtual_address, region.page_count)),
            )?,
        };

        if self.mmap_regions.len() >= MAX_MMAP_REGIONS {
            return None;
        }
        let span = page_count.checked_mul(PAGE_SIZE)?;

        // Per-task memory quota: reject before any frame or PTE is touched, so
        // a refused request leaves no state behind.
        if !silver_microkernel::mmap_quota_ok(self.mmap_bytes, span, MAX_MMAP_BYTES_PER_TASK) {
            return None;
        }

        // Charge the data pages before the frame allocation. Page tables added
        // beyond the fixed space cost at most one L3 per 2 MiB (0.2%), which
        // the flat `SPACE_TABLE_FRAMES` charge covers for budgeting purposes.
        // A lazy mapping commits nothing here: each page is charged when it
        // faults in, so an untouched 128 MiB reservation costs only its VA.
        if !lazy && !self.charge_frames(page_count) {
            return None;
        }
        Some((virtual_address, page_count, span))
    }

    /// Allocates and maps `length` bytes into the task's TTBR0. Anonymous
    /// mappings pass `fixed = None`; file-backed mappings may request an exact
    /// address (Linux `MAP_FIXED`) and pass the file bytes to copy into the
    /// freshly allocated frames.
    pub(super) fn mmap_region(
        &mut self,
        length: usize,
        prot: u64,
        fixed: Option<usize>,
        contents: Option<&[u8]>,
        shared: Option<(usize, SharedMapping)>,
        lazy: bool,
    ) -> Option<usize> {
        let (virtual_address, page_count, span) =
            self.mmap_prepare(length, prot, fixed, lazy)?;
        if lazy {
            let region = MmapRegion {
                virtual_address,
                page_count,
                physical_start: 0,
                generation: 0,
                prot,
                shared: SharedMapping::default(),
                frames: Vec::new(),
                lazy: true,
                pages: BTreeMap::new(),
                file: None,
            };
            let position = self
                .mmap_regions
                .iter()
                .position(|existing| existing.virtual_address > virtual_address)
                .unwrap_or(self.mmap_regions.len());
            self.mmap_regions.insert(position, region);
            self.mmap_bytes += span;
            return Some(virtual_address);
        }
        let (physical_start, generation, owner) = match shared {
            // Frames already owned by a memfd: map them where they are and
            // keep the lifetime handle. They must never be freed here.
            Some((physical_start, owner)) => (physical_start, 0u32, owner),
            None => {
                let (physical_start, generation) =
                    match frame_allocator::alloc_user_contiguous_pages_tagged(page_count) {
                        Some(pair) => pair,
                        None => {
                            self.release_frames(page_count);
                            return None;
                        }
                    };
                (physical_start, generation, SharedMapping::default())
            }
        };
        let mapped = if owner.is_some() {
            mmu::map_shared_writable_pages_at(
                self.ttbr0,
                virtual_address,
                physical_start,
                page_count,
                prot,
            )
        } else {
            mmu::map_user_pages_at(
                self.ttbr0,
                virtual_address,
                physical_start,
                page_count,
                prot,
            )
        };
        if !mapped {
            if !owner.is_some() {
                frame_allocator::free_contiguous_pages_tagged(
                    physical_start,
                    page_count,
                    generation,
                );
            }
            self.release_frames(page_count);
            return None;
        }
        // The mapping is zeroed by `map_user_pages_at`; copy only the file
        // bytes that exist. A shared window already holds the memfd's bytes.
        if let Some(bytes) = contents {
            if !owner.is_some() {
                let count = bytes.len().min(span);
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        bytes.as_ptr(),
                        phys_to_virt(physical_start) as *mut u8,
                        count,
                    );
                }
            }
        }

        #[cfg(feature = "gdb-stub")]
        {
            let watch_page = crate::debug::soft_page() as usize;
            let end = virtual_address + page_count * crate::memory::PAGE_SIZE;
            if crate::debug::soft_armed() && watch_page >= virtual_address && watch_page < end {
                let physical = physical_start + (watch_page - virtual_address);
                let _ = mmu::map_user_pages_at(
                    self.ttbr0,
                    watch_page,
                    physical,
                    1,
                    prot & !mmu::PROT_WRITE,
                );
            }
        }
        let region = MmapRegion {
            virtual_address,
            page_count,
            physical_start,
            generation,
            prot,
            shared: owner,
            frames: Vec::new(),
            lazy: false,
            pages: BTreeMap::new(),
            file: None,
        };
        let position = self
            .mmap_regions
            .iter()
            .position(|existing| existing.virtual_address > virtual_address)
            .unwrap_or(self.mmap_regions.len());
        self.mmap_regions.insert(position, region);
        self.mmap_bytes += span;
        Some(virtual_address)
    }

    /// Maps a file-backed `MAP_PRIVATE` range one frame per page. `fill` is
    /// called with the mapping-relative offset and a zeroed page buffer; it
    /// returns `false` to abort (an I/O error must not publish a half image).
    /// Reserves address space for a file mapping; each page is read from
    /// `path` when it faults. A library segment is typically 100+ MiB while
    /// only a fraction is executed, and an eager copy cost 15 MB/s through
    /// the block device.
    pub(super) fn mmap_file_lazy(
        &mut self,
        length: usize,
        prot: u64,
        fixed: Option<usize>,
        path: alloc::string::String,
        file_offset: usize,
    ) -> Option<usize> {
        let (virtual_address, page_count, span) =
            self.mmap_prepare(length, prot, fixed, true)?;
        let region = MmapRegion {
            virtual_address,
            page_count,
            physical_start: 0,
            generation: 0,
            prot,
            shared: SharedMapping::default(),
            frames: Vec::new(),
            lazy: true,
            pages: BTreeMap::new(),
            file: Some((path, file_offset)),
        };
        let position = self
            .mmap_regions
            .iter()
            .position(|existing| existing.virtual_address > virtual_address)
            .unwrap_or(self.mmap_regions.len());
        self.mmap_regions.insert(position, region);
        self.mmap_bytes += span;
        Some(virtual_address)
    }

    pub(super) fn mmap_file_paged<F>(
        &mut self,
        length: usize,
        prot: u64,
        fixed: Option<usize>,
        mut fill: F,
    ) -> Option<usize>
    where
        F: FnMut(usize, &mut [u8]) -> bool,
    {
        let (virtual_address, page_count, span) =
            self.mmap_prepare(length, prot, fixed, false)?;
        let mut frames: Vec<(usize, u32)> = Vec::new();
        if frames.try_reserve(page_count).is_err() {
            self.release_frames(page_count);
            return None;
        }
        // Read the file in runs instead of page by page: every `fill` call
        // turns into an SFS read, and a library segment like libLLVM (163 MiB)
        // is 40k pages. Page-sized reads made the mapping take fifteen
        // seconds because the block cache (256 KiB) never had a chance to
        // help; 64 KiB runs cut the number of file reads by sixteen.
        const BATCH_PAGES: usize = 16;
        let mut buffer = alloc::vec![0_u8; PAGE_SIZE * BATCH_PAGES];
        let mut index = 0usize;
        while index < page_count {
            let batch = (page_count - index).min(BATCH_PAGES);
            let bytes = batch * PAGE_SIZE;
            for byte in buffer[..bytes].iter_mut() {
                *byte = 0;
            }
            if !fill(index * PAGE_SIZE, &mut buffer[..bytes]) {
                self.abort_paged_mapping(virtual_address, index, &mut frames, page_count);
                return None;
            }
            for slot in 0..batch {
                let Some((physical, generation)) = frame_allocator::alloc_user_page_tagged() else {
                    self.abort_paged_mapping(virtual_address, index + slot, &mut frames, page_count);
                    return None;
                };
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        buffer[slot * PAGE_SIZE..].as_ptr(),
                        phys_to_virt(physical) as *mut u8,
                        PAGE_SIZE,
                    );
                }
                if !mmu::map_user_page_at(
                    self.ttbr0,
                    virtual_address + (index + slot) * PAGE_SIZE,
                    physical,
                    prot,
                ) {
                    let _ = frame_allocator::free_page_tagged(physical, generation);
                    self.abort_paged_mapping(
                        virtual_address,
                        index + slot,
                        &mut frames,
                        page_count,
                    );
                    return None;
                }
                frames.push((physical, generation));
            }
            index += batch;
        }
        let region = MmapRegion {
            virtual_address,
            page_count,
            physical_start: 0,
            generation: 0,
            prot,
            shared: SharedMapping::default(),
            frames,
            lazy: false,
            pages: BTreeMap::new(),
            file: None,
        };
        let position = self
            .mmap_regions
            .iter()
            .position(|existing| existing.virtual_address > virtual_address)
            .unwrap_or(self.mmap_regions.len());
        self.mmap_regions.insert(position, region);
        self.mmap_bytes += span;
        Some(virtual_address)
    }

    /// Rolls back the already-published prefix of a paged file mapping.
    pub(super) fn abort_paged_mapping(
        &mut self,
        virtual_address: usize,
        mapped: usize,
        frames: &mut Vec<(usize, u32)>,
        charged: usize,
    ) {
        if mapped > 0 && !mmu::unmap_user_pages_at(self.ttbr0, virtual_address, mapped) {
            MMAP_LEAKED_FRAMES.fetch_add(mapped, Ordering::Relaxed);
            console::write_str("MMAP LEAK: rollback could not unmap range 0x");
            console::write_hex_u64(virtual_address as u64);
            console::write_str("\r\n");
        }
        for (page, generation) in frames.drain(..) {
            let _ = frame_allocator::free_page_tagged(page, generation);
        }
        self.release_frames(charged);
    }

    pub(super) fn mmap_anonymous(&mut self, length: usize, prot: u64) -> Option<usize> {
        self.mmap_region(length, prot, None, None, None, false)
    }

    pub(super) fn mmap_file(
        &mut self,
        length: usize,
        prot: u64,
        fixed: Option<usize>,
        contents: &[u8],
    ) -> Option<usize> {
        self.mmap_region(length, prot, fixed, Some(contents), None, false)
    }

    /// Maps a memfd `MAP_SHARED` window over frames owned by the memfd.
    #[cfg(feature = "linux-abi")]
    pub(super) fn mmap_shared_file(
        &mut self,
        length: usize,
        prot: u64,
        fixed: Option<usize>,
        physical_start: usize,
        owner: crate::linux_fd::MappedFile,
    ) -> Option<usize> {
        self.mmap_region(
            length,
            prot,
            fixed,
            None,
            Some((
                physical_start,
                SharedMapping {
                    owner: Some(owner),
                },
            )),
            false,
        )
    }

    /// Returns an RX page containing `mov x8, #139; svc #0`, the user trampoline
    /// used when a `sigaction` has no `SA_RESTORER`. It is created once per
    /// task and cached in `signal_trampoline`; because it is an ordinary mmap
    /// region, fork copies it and teardown frees its frame.
    #[cfg(feature = "linux-abi")]
    pub(super) fn signal_trampoline(&mut self) -> Option<usize> {
        if let Some(address) = self.signal_trampoline {
            return Some(address);
        }
        // mov x8, #139 (rt_sigreturn); svc #0; brk #0.
        const CODE: [u8; 12] = [
            0x68, 0x11, 0x80, 0xd2, 0x01, 0x00, 0x00, 0xd4, 0x00, 0x00, 0x20, 0xd4,
        ];
        let address =
            self.mmap_region(PAGE_SIZE, mmu::PROT_EXEC, None, Some(&CODE), None, false)?;
        self.signal_trampoline = Some(address);
        Some(address)
    }

    /// Backs one page of a lazy anonymous region. `esr` decides whether a
    /// write is permitted; untouched pages are zero-filled.
    #[cfg(feature = "linux-abi")]
    /// Makes the watched page writable again so an unrelated store in the
    /// same page can complete (the page-level software watch is one-shot).
    #[cfg(feature = "gdb-stub")]
    pub(super) fn allow_watch_page(&mut self, page: usize) -> bool {
        let ttbr0 = self.ttbr0;
        for region in &self.mmap_regions {
            let start = region.virtual_address;
            let end = start + region.page_count * crate::memory::PAGE_SIZE;
            if page < start || page >= end {
                continue;
            }
            let physical = if region.lazy {
                let offset = ((page - start) / crate::memory::PAGE_SIZE) as u32;
                match region.pages.get(&offset) {
                    Some((physical, _)) => *physical,
                    None => return false,
                }
            } else {
                region.physical_start + (page - start)
            };
            return mmu::map_user_page_at(ttbr0, page, physical, region.prot);
        }
        false
    }

    /// Log-mode variant: lets the store through and remembers the mapping so
    /// the next SVC re-protects the watched page (see `debug::reprotect_soft_page`).
    #[cfg(feature = "gdb-stub")]
    pub(super) fn allow_watch_page_logged(&mut self, page: usize) -> bool {
        let ttbr0 = self.ttbr0;
        for region in &self.mmap_regions {
            let start = region.virtual_address;
            let end = start + region.page_count * crate::memory::PAGE_SIZE;
            if page < start || page >= end {
                continue;
            }
            let physical = if region.lazy {
                let offset = ((page - start) / crate::memory::PAGE_SIZE) as u32;
                match region.pages.get(&offset) {
                    Some((physical, _)) => *physical,
                    None => return false,
                }
            } else {
                region.physical_start + (page - start)
            };
            crate::debug::request_reprotect(
                ttbr0 as u64,
                page as u64,
                physical as u64,
                region.prot as u64,
            );
            return mmu::map_user_page_at(ttbr0, page, physical, region.prot);
        }
        // Private Linux image run pages (already COWed to `Data`): the same
        // allow/re-protect dance works on the task's own frame. A still-COW
        // page must not be shared-written, so it falls through to the normal
        // copy path, which re-protects the page itself.
        let writable = mmu::PROT_READ | mmu::PROT_WRITE;
        #[cfg(feature = "linux-abi")]
        if let Some(image) = self.linux_image.as_ref() {
            for run in &image.runs {
                if page < run.virtual_address
                    || page >= run.virtual_address + run.page_count * PAGE_SIZE
                {
                    continue;
                }
                let index = (page - run.virtual_address) / PAGE_SIZE;
                if run.kinds[index] != crate::elf::PageKind::Data {
                    return false;
                }
                let physical = run.physical_start + (page - run.virtual_address);
                crate::debug::request_reprotect(
                    ttbr0 as u64,
                    page as u64,
                    physical as u64,
                    writable,
                );
                return mmu::map_user_page_at(ttbr0, page, physical, writable);
            }
        }
        false
    }

    pub(super) fn lazy_page_fault(&mut self, index: usize, page: usize, esr: u64) -> bool {
        const WNR: u64 = 1 << 6;
        macro_rules! lazy_fail {
            ($reason:expr) => {{
                crate::scheduler::lazy_fail_record(page, $reason, self.mem_frames, self.mmap_bytes, self.mmap_regions.len());
                return false;
            }};
        }
        let prot = self.mmap_regions[index].prot;
        let write = esr & WNR != 0;
        if prot == 0 || (write && prot & mmu::PROT_WRITE == 0) {
            lazy_fail!(b"no-prot");
        }
        let offset = ((page - self.mmap_regions[index].virtual_address) / PAGE_SIZE) as u32;
        if self.mmap_regions[index].pages.contains_key(&offset) {
            // Another thread backed this page while this one was on its way
            // to the fault handler. The mapping exists, so the retry must
            // succeed; killing the faulting task here would murder a healthy
            // thread racing a sibling on a shared, lazily mapped region.
            return true;
        }
        let source = self.mmap_regions[index].file.clone();
        if !self.charge_frames(1) {
            lazy_fail!(b"charge");
        }
        let Some((physical, generation)) = frame_allocator::alloc_user_page_tagged() else {
            self.release_frames(1);
            lazy_fail!(b"alloc");
        };
        unsafe {
            core::ptr::write_bytes(phys_to_virt(physical) as *mut u8, 0, PAGE_SIZE);
        }
        if let Some((path, file_offset)) = source {
            // The faulting thread waits for one page of disk I/O; that is the
            // price of not copying an unused library into frames. VFS reads
            // are polled and take the VFS lock, which no path holds while
            // waiting for the scheduler lock.
            let start = file_offset + offset as usize * PAGE_SIZE;
            match crate::vfs::read_path_at(&path, start, PAGE_SIZE) {
                Ok(bytes) => {
                    let count = bytes.len().min(PAGE_SIZE);
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            bytes.as_ptr(),
                            phys_to_virt(physical) as *mut u8,
                            count,
                        );
                    }
                }
                Err(()) => {
                    // Mapping zeros would silently turn an I/O error into an
                    // empty page; fail the access instead.
                    let _ = frame_allocator::free_page_tagged(physical, generation);
                    self.release_frames(1);
                    lazy_fail!(b"io");
                }
            }
        }
        if !mmu::map_user_page_at(self.ttbr0, page, physical, prot) {
            frame_allocator::free_page_tagged(physical, generation);
            self.release_frames(1);
            lazy_fail!(b"map");
        }
        #[cfg(feature = "gdb-stub")]
        if crate::debug::soft_page_is(page as u64) {
            let _ = mmu::map_user_page_at(
                self.ttbr0,
                page,
                physical,
                prot & !mmu::PROT_WRITE,
            );
        }
        self.mmap_regions[index]
            .pages
            .insert(offset, (physical, generation));
        let index2 = LAZY_OK_INDEX.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        unsafe {
            let ring = core::ptr::addr_of_mut!(LAZY_OK);
            (*ring)[index2 % LAZY_OK_LEN] = [
                page as u64,
                prot,
                physical as u64,
                current_task_id(),
                0,
                0,
            ];
        }
        true
    }

    /// Handles a write to a page shared by `fork`. Copies the containing run,
    /// remaps it writable in this address space and drops this task's reference
    /// to the old run. A write to a genuinely read-only page returns `false`,
    /// which the fault path turns into the normal task kill.
    #[cfg(feature = "linux-abi")]
    pub(super) fn cow_fault(&mut self, address: usize, esr: u64) -> bool {
        const WNR: u64 = 1 << 6;
        cow_reason_set(b"enter");
        if self.ttbr0 == 0 {
            cow_reason_set(b"no-ttbr0");
            return false;
        }
        // Kernel-initiated copies can pass a kernel VA (a corrupted user
        // register fed the fault dump's `copy_from_user`, for example). A
        // page-table walk on such an address indexes far past the L2 table
        // and faults inside the fault handler itself. Linux images may live
        // well below DRAM_BASE (the loader honours the ELF's own bias), so
        // only the kernel VA range is rejected here.
        if (address as u64) >= (1_u64 << 48) {
            return false;
        }
        let page = address & !(PAGE_SIZE - 1);
        let ttbr0 = self.ttbr0;
        #[cfg(feature = "gdb-stub")]
        if esr & WNR != 0 && crate::debug::soft_page_is(page as u64) {
            if crate::debug::fault_context_valid() {
                if crate::debug::soft_contains(address as u64) {
                    if crate::debug::log_mode() {
                        crate::debug::log_soft_hit(true);
                        if self.allow_watch_page_logged(page) {
                            return true;
                        }
                        // Not a pre-mapped page (still-COW image run, stack):
                        // fall through to the normal handling below, which
                        // copies the page and re-protects it in log mode.
                    } else {
                        crate::debug::note_soft_hit();
                        return false;
                    }
                } else if crate::debug::log_mode() {
                    crate::debug::log_soft_hit(false);
                    if self.allow_watch_page_logged(page) {
                        return true;
                    }
                }
            } else {
                // A kernel-initiated copy (memfd window, fork) cannot report
                // a user PC; let it through without disturbing the watch.
                if crate::debug::log_mode() {
                    if self.allow_watch_page_logged(page) {
                        return true;
                    }
                }
            }
            // Another field in the same page: let the store through. The
            // target field keeps its watch only if it was written first; a
            // fine-grained watch needs hardware support.
            if !crate::debug::log_mode() {
                return self.allow_watch_page(page);
            }
        }

        // A lazily reserved anonymous page backs itself on first access,
        // read or write. This is how a large `PROT_NONE` reservation from
        // glibc becomes a usable heap after `mprotect`.
        if let Some(index) = self.mmap_regions.iter().position(|region| {
            region.lazy
                && page >= region.virtual_address
                && page < region.virtual_address + region.page_count * PAGE_SIZE
        }) {
            return self.lazy_page_fault(index, page, esr);
        }

        if esr & WNR == 0 {
            cow_reason_set(b"read-fault-no-handler");
            return false;
        }

        if let Some(image) = self.linux_image.as_mut() {
            if let Some(run) = image.runs.iter_mut().find(|run| {
                page >= run.virtual_address
                    && page < run.virtual_address + run.page_count * PAGE_SIZE
            }) {
                let index = (page - run.virtual_address) / PAGE_SIZE;
                if !matches!(
                    run.kinds[index],
                    crate::elf::PageKind::Cow | crate::elf::PageKind::Data
                ) {
                    cow_reason_set(b"img refuse-kind");
                    return false;
                }
                let old_physical = run.physical_start;
                let old_generation = run.generation;
                let count = run.page_count;
                let virtual_address = run.virtual_address;
                let Some((new_physical, new_generation)) =
                    cow_copy_run(old_physical, count, old_generation)
                else {
                    cow_reason_set(b"img copy-fail");
                    return false;
                };
                let mut kinds = run.kinds.clone();
                for kind in &mut kinds {
                    if *kind == crate::elf::PageKind::Cow {
                        *kind = crate::elf::PageKind::Data;
                    }
                }
                if !mmu::remap_user_kinds_at(ttbr0, virtual_address, new_physical, &kinds) {
                    cow_reason_set(b"img remap-fail");
                    frame_allocator::free_contiguous_pages_tagged(
                        new_physical,
                        count,
                        new_generation,
                    );
                    return false;
                }
                frame_allocator::free_contiguous_pages_tagged(
                    old_physical,
                    count,
                    old_generation,
                );
                run.physical_start = new_physical;
                run.generation = new_generation;
                run.kinds = kinds;
                // Log mode: the just-remapped page is writable; put the watch
                // back on the single faulting page so the next write logs and
                // the SVC hook re-protects it again.
                #[cfg(feature = "gdb-stub")]
                if crate::debug::log_mode()
                    && crate::debug::fault_context_valid()
                    && crate::debug::soft_page_is(page as u64)
                {
                    let writable = mmu::PROT_READ | mmu::PROT_WRITE;
                    let physical = new_physical + (page - run.virtual_address);
                    let _ = mmu::map_user_page_at(
                        ttbr0,
                        page,
                        physical,
                        writable & !mmu::PROT_WRITE,
                    );
                    crate::debug::request_reprotect(
                        ttbr0 as u64,
                        page as u64,
                        physical as u64,
                        writable,
                    );
                }
                return true;
            }
        }

        if page >= mmu::USER_STACK_VA && page < mmu::USER_STACK_VA + TASK_USER_STACK_SIZE {
            let old_physical = self.user_stack.start;
            let old_generation = self.user_stack_generation;
            let Some((new_physical, new_generation)) =
                cow_copy_run(old_physical, TASK_USER_STACK_PAGES, old_generation)
            else {
                return false;
            };
            if !mmu::remap_user_pages_at(
                ttbr0,
                mmu::USER_STACK_VA,
                new_physical,
                TASK_USER_STACK_PAGES,
                mmu::PROT_WRITE,
            ) {
                frame_allocator::free_contiguous_pages_tagged(
                    new_physical,
                    TASK_USER_STACK_PAGES,
                    new_generation,
                );
                return false;
            }
            frame_allocator::free_contiguous_pages_tagged(
                old_physical,
                TASK_USER_STACK_PAGES,
                old_generation,
            );
            self.user_stack = MemoryRegion::new(
                new_physical,
                new_physical + TASK_USER_STACK_SIZE,
            );
            self.user_stack_generation = new_generation;
            return true;
        }

        if page == mmu::PRIVATE_WINDOW_VA {
            let old_physical = self.private_page;
            let old_generation = self.private_page_generation;
            let Some((new_physical, new_generation)) =
                cow_copy_run(old_physical, 1, old_generation)
            else {
                return false;
            };
            if !mmu::remap_private_page(ttbr0, new_physical) {
                frame_allocator::free_contiguous_pages_tagged(
                    new_physical,
                    1,
                    new_generation,
                );
                return false;
            }
            frame_allocator::free_contiguous_pages_tagged(old_physical, 1, old_generation);
            self.private_page = new_physical;
            self.private_page_generation = new_generation;
            return true;
        }

        if page >= mmu::USER_HEAP_VA {
            let index = (page - mmu::USER_HEAP_VA) / PAGE_SIZE;
            if index < self.heap_pages.len() {
                let (old_physical, old_generation) = self.heap_pages[index];
                let Some((new_physical, new_generation)) =
                    cow_copy_run(old_physical, 1, old_generation)
                else {
                    return false;
                };
                if !mmu::remap_user_pages_at(
                    ttbr0,
                    mmu::USER_HEAP_VA + index * PAGE_SIZE,
                    new_physical,
                    1,
                    mmu::PROT_WRITE,
                ) {
                    frame_allocator::free_contiguous_pages_tagged(
                        new_physical,
                        1,
                        new_generation,
                    );
                    return false;
                }
                frame_allocator::free_contiguous_pages_tagged(
                    old_physical,
                    1,
                    old_generation,
                );
                self.heap_pages[index] = (new_physical, new_generation);
                return true;
            }
        }

        if let Some(region_index) = self.mmap_regions.iter().position(|region| {
            page >= region.virtual_address
                && page < region.virtual_address + region.page_count * PAGE_SIZE
        }) {
            let region = &self.mmap_regions[region_index];
            // A MAP_SHARED window owns no frames: a write fault means the
            // window was made read-only, and Linux kills the task instead of
            // silently breaking the sharing contract.
            if region.shared.is_some() || region.prot & mmu::PROT_WRITE == 0 {
                return false;
            }
            if !region.frames.is_empty() {
                let page_index = (page - region.virtual_address) / PAGE_SIZE;
                let prot = region.prot;
                let (old_physical, old_generation) = region.frames[page_index];
                let Some((new_physical, new_generation)) =
                    cow_copy_run(old_physical, 1, old_generation)
                else {
                    return false;
                };
                // The slot is already mapped (read-only); `map_user_page_at`
                // refuses existing mappings, so re-point it instead.
                if !mmu::remap_user_pages_at(ttbr0, page, new_physical, 1, prot) {
                    frame_allocator::free_page_tagged(new_physical, new_generation);
                    return false;
                }
                frame_allocator::free_page_tagged(old_physical, old_generation);
                self.mmap_regions[region_index].frames[page_index] =
                    (new_physical, new_generation);
                return true;
            }
            let old_physical = region.physical_start;
            let old_generation = region.generation;
            let count = region.page_count;
            let virtual_address = region.virtual_address;
            let prot = region.prot;
            let Some((new_physical, new_generation)) =
                cow_copy_run(old_physical, count, old_generation)
            else {
                return false;
            };
            if !mmu::remap_user_pages_at(
                ttbr0,
                virtual_address,
                new_physical,
                count,
                prot,
            ) {
                frame_allocator::free_contiguous_pages_tagged(
                    new_physical,
                    count,
                    new_generation,
                );
                return false;
            }
            frame_allocator::free_contiguous_pages_tagged(
                old_physical,
                count,
                old_generation,
            );
            let region = &mut self.mmap_regions[region_index];
            region.physical_start = new_physical;
            region.generation = new_generation;
            return true;
        }

        false
    }

    /// Unmaps any page-aligned range, even when it spans several mappings or
    /// holes. Used by `munmap(2)` and by `MAP_FIXED` replacement.
    pub(super) fn munmap_range(&mut self, address: usize, length: usize) -> bool {
        let page_count = length.div_ceil(PAGE_SIZE);
        // Same user-controlled bound as `mmap_region`: an absurd length must
        // be rejected up front, before the unmap loop walks pages.
        if page_count == 0 || page_count > mmu::USER_MMAP_MAX_PAGES || address % PAGE_SIZE != 0 {
            return false;
        }
        let Some(span) = page_count.checked_mul(PAGE_SIZE) else {
            return false;
        };
        let Some(end) = address.checked_add(span) else {
            return false;
        };
        if address < mmu::USER_MMAP_VA || end > mmu::USER_MMAP_MAX_VA {
            return false;
        }

        // Walk backwards: removing or inserting a region only shifts entries
        // above the current index, and those were already processed.
        let mut index = self.mmap_regions.len();
        while index > 0 {
            index -= 1;
            let region_start = self.mmap_regions[index].virtual_address;
            let region_end = region_start + self.mmap_regions[index].page_count * PAGE_SIZE;
            let inter_start = address.max(region_start);
            let inter_end = end.min(region_end);
            if inter_start >= inter_end {
                continue;
            }
            let start_page = (inter_start - region_start) / PAGE_SIZE;
            let count = (inter_end - inter_start) / PAGE_SIZE;
            let physical_start = self.mmap_regions[index].physical_start;
            let generation = self.mmap_regions[index].generation;
            let prot = self.mmap_regions[index].prot;
            let region_pages = self.mmap_regions[index].page_count;
            let shared = self.mmap_regions[index].shared.is_some();
            let left = start_page;
            let right = region_pages - start_page - count;
            // A middle range needs one extra bookkeeping slot. Refuse before
            // any PTE or frame is touched, so a full table leaves the mapping
            // intact.
            if left > 0 && right > 0 && self.mmap_regions.len() >= MAX_MMAP_REGIONS {
                return false;
            }
            let paged = !self.mmap_regions[index].frames.is_empty();
            let lazy = self.mmap_regions[index].lazy;
            let removed_physical = physical_start + start_page * PAGE_SIZE;

            // Fail closed: `unmap_user_pages_at` is all-or-nothing, so on
            // failure nothing was unmapped and the region stays tracked. A
            // lazy region has holes and uses the lenient walk.
            let unmapped = if lazy {
                mmu::unmap_user_pages_at_lenient(self.ttbr0, inter_start, count)
            } else {
                mmu::unmap_user_pages_at(self.ttbr0, inter_start, count)
            };
            if !unmapped {
                MMAP_LEAKED_FRAMES.fetch_add(count, Ordering::Relaxed);
                console::write_str("MMAP LEAK: range 0x");
                console::write_hex_u64(if lazy {
                    inter_start as u64
                } else {
                    removed_physical as u64
                });
                console::write_str(" left mapped after refused unmap (");
                console::write_dec_u64(count as u64);
                console::write_str(" pages)\r\n");
                return false;
            }
            if lazy {
                // Only touched pages were ever charged; release exactly the
                // frames inside the removed range.
                let freed = self.mmap_regions[index]
                    .free_pages_range(start_page as u32, count as u32);
                self.release_frames(freed);
            } else if paged {
                // One frame at a time: that is how a file-backed segment was
                // allocated in the first place.
                let removed: Vec<(usize, u32)> = self.mmap_regions[index]
                    .frames
                    .drain(start_page..start_page + count)
                    .collect();
                for (page, frame_generation) in removed {
                    if !frame_allocator::free_page_tagged(page, frame_generation) {
                        console::write_str("MMAP: allocator refused page 0x");
                        console::write_hex_u64(page as u64);
                        console::write_str(" - leaked\r\n");
                    }
                }
            } else if !shared
                && !frame_allocator::free_contiguous_pages_tagged(
                    removed_physical,
                    count,
                    generation,
                )
            {
                // Free only the removed slice of the original contiguous run;
                // every page in it shares the run's generation. A shared memfd
                // window leaves its frames to the memfd object.
                console::write_str("MMAP: allocator refused run 0x");
                console::write_hex_u64(removed_physical as u64);
                console::write_str(" - leaked\r\n");
            }

            match (left > 0, right > 0) {
                (false, false) => {
                    self.mmap_regions.remove(index);
                }
                (true, false) => {
                    let region = &mut self.mmap_regions[index];
                    region.page_count = left;
                    region.prot = prot;
                }
                (false, true) => {
                    let region = &mut self.mmap_regions[index];
                    region.virtual_address = inter_end;
                    region.page_count = right;
                    region.prot = prot;
                    if lazy {
                        // Page offsets are relative to the region start.
                        let shift = (start_page + count) as u32;
                        let pages = core::mem::take(&mut region.pages);
                        for (key, value) in pages {
                            region.pages.insert(key - shift, value);
                        }
                    } else if !paged {
                        region.physical_start = removed_physical + count * PAGE_SIZE;
                    }
                }
                (true, true) => {
                    let mut tail = self.mmap_regions[index].clone();
                    tail.virtual_address = inter_end;
                    tail.page_count = right;
                    tail.prot = prot;
                    if lazy {
                        tail.pages = self.mmap_regions[index]
                            .take_pages_from((start_page + count) as u32);
                    } else if paged {
                        // The drain above left left+right frames; split the
                        // tail off after the retained prefix.
                        tail.frames = self.mmap_regions[index].frames.split_off(start_page);
                    } else {
                        tail.physical_start = removed_physical + count * PAGE_SIZE;
                    }
                    self.mmap_regions[index].page_count = left;
                    self.mmap_regions.insert(index + 1, tail);
                }
            }
            self.mmap_bytes = self.mmap_bytes.saturating_sub(count * PAGE_SIZE);
            if !lazy {
                self.release_frames(count);
            }
        }
        true
    }

    pub(super) fn munmap(&mut self, address: usize, length: usize) -> bool {
        self.munmap_range(address, length)
    }

    /// Changes protections on an already-mapped range. `mprotect(2)` may cover
    /// several adjacent regions, so the range is split at its boundaries and
    /// every fully covered region records the new `prot` for `fork`.
    pub(super) fn mprotect_range(&mut self, address: usize, length: usize, prot: u64) -> bool {
        if prot & mmu::PROT_WRITE != 0 && prot & mmu::PROT_EXEC != 0 {
            return false;
        }
        let page_count = length.div_ceil(PAGE_SIZE);
        if page_count == 0 || page_count > mmu::USER_MMAP_MAX_PAGES || address % PAGE_SIZE != 0 {
            return false;
        }
        let Some(span) = page_count.checked_mul(PAGE_SIZE) else {
            return false;
        };
        let Some(end) = address.checked_add(span) else {
            return false;
        };

        // Anonymous/file-backed mappings use the region list so `fork` and
        // `munmap` see the updated protection.
        if address >= mmu::USER_MMAP_VA && end <= mmu::USER_MMAP_MAX_VA {
            // Every page in the requested range must already belong to a
            // mapping; unlike Linux we do not create PROT_NONE holes
            // implicitly.
            let mut cursor = address;
            while cursor < end {
                let mut next = 0;
                for region in &self.mmap_regions {
                    let region_end =
                        region.virtual_address + region.page_count * PAGE_SIZE;
                    if cursor >= region.virtual_address && cursor < region_end {
                        next = region_end;
                        break;
                    }
                }
                if next == 0 {
                    return false;
                }
                cursor = next.min(end);
            }
            // Only a boundary strictly inside a region needs a split. Most
            // `mprotect` calls are page-aligned with an existing region and
            // need none, so do not reserve two slots unconditionally: with a
            // full table that would reject a no-op protection change.
            let mut splits = 0;
            for boundary in [address, end] {
                if self.mmap_regions.iter().any(|region| {
                    let region_end =
                        region.virtual_address + region.page_count * PAGE_SIZE;
                    region.virtual_address < boundary && boundary < region_end
                }) {
                    splits += 1;
                }
            }
            if self.mmap_regions.len() + splits > MAX_MMAP_REGIONS {
                return false;
            }
            // Lazy reservations may not have a frame behind every page yet;
            // a lenient walk records the protection for the ones that do.
            if !mmu::protect_user_pages_at_lenient(self.ttbr0, address, page_count, prot) {
                return false;
            }
            if !self.split_region_at(address) || !self.split_region_at(end) {
                return false;
            }
            for region in &mut self.mmap_regions {
                let region_end = region.virtual_address + region.page_count * PAGE_SIZE;
                if region.virtual_address >= address && region_end <= end {
                    region.prot = prot;
                }
            }
            return true;
        }

        // The main executable and its interpreter live below the mmap window
        // (MAIN_BIAS/INTERP_BIAS). glibc calls mprotect on the main binary's
        // RELRO immediately after relocation, so that range must work too.
        #[cfg(feature = "linux-abi")]
        {
            let ttbr0 = self.ttbr0;
            let Some(image) = self.linux_image.as_mut() else {
                return false;
            };
            let mut cursor = address;
            while cursor < end {
                let mut next = 0;
                for run in &image.runs {
                    let run_end = run.virtual_address + run.page_count * PAGE_SIZE;
                    if cursor >= run.virtual_address && cursor < run_end {
                        next = run_end;
                        break;
                    }
                }
                if next == 0 {
                    return false;
                }
                cursor = next.min(end);
            }
            if !mmu::protect_user_pages_at(ttbr0, address, page_count, prot) {
                return false;
            }
            // Keep `kinds` in sync so `fork` restores the final protections.
            let kind = if prot & mmu::PROT_EXEC != 0 {
                crate::elf::PageKind::Text
            } else if prot & mmu::PROT_WRITE != 0 {
                crate::elf::PageKind::Data
            } else {
                crate::elf::PageKind::ReadOnly
            };
            for run in &mut image.runs {
                let run_start = run.virtual_address;
                let run_end = run_start + run.page_count * PAGE_SIZE;
                let inter_start = address.max(run_start);
                let inter_end = end.min(run_end);
                if inter_start >= inter_end {
                    continue;
                }
                let first = (inter_start - run_start) / PAGE_SIZE;
                let last = (inter_end - run_start) / PAGE_SIZE;
                for slot in &mut run.kinds[first..last] {
                    *slot = kind;
                }
            }
            true
        }
        #[cfg(not(feature = "linux-abi"))]
        {
            let _ = (address, end, page_count);
            false
        }
    }

    pub(super) fn split_region_at(&mut self, address: usize) -> bool {
        let Some(index) = self.mmap_regions.iter().position(|region| {
            let end = region.virtual_address + region.page_count * PAGE_SIZE;
            region.virtual_address < address && address < end
        }) else {
            return true;
        };
        if self.mmap_regions.len() >= MAX_MMAP_REGIONS {
            return false;
        }
        let offset_pages =
            (address - self.mmap_regions[index].virtual_address) / PAGE_SIZE;
        let mut tail = self.mmap_regions[index].clone();
        tail.virtual_address = address;
        tail.page_count = self.mmap_regions[index].page_count - offset_pages;
        if tail.lazy {
            tail.pages = self.mmap_regions[index].take_pages_from(offset_pages as u32);
            // A file-backed region reads page `n` at `file_offset + n * PAGE`.
            // The tail's page 0 is `offset_pages` further into the file.
            if let Some((_, file_offset)) = tail.file.as_mut() {
                *file_offset += offset_pages * PAGE_SIZE;
            }
        } else if tail.frames.is_empty() {
            tail.physical_start =
                self.mmap_regions[index].physical_start + offset_pages * PAGE_SIZE;
        } else {
            tail.frames = tail.frames.split_off(offset_pages);
            self.mmap_regions[index].frames.truncate(offset_pages);
        }
        self.mmap_regions[index].page_count = offset_pages;
        self.mmap_regions.insert(index + 1, tail);
        true
    }

    pub(super) fn release_mutexes(&mut self) -> Vec<u64> {
        let mut wake = Vec::new();
        for state in self.owned_mutexes.drain(..) {
            if let Some(waiter) = unsafe { mutex::release_owner(state, self.id) } {
                wake.push(waiter);
            }
        }
        wake
    }

    pub(super) fn release_stack(&self) {
        if self.resources_released {
            return;
        }
        if !kernel_stack::free(self.stack.start) {
            console::write_str("STACK: kernel stack release refused for 0x");
            console::write_hex_u64(self.stack.start as u64);
            console::write_str(" - leaked\r\n");
        }
        if !self.user_stack.is_empty()
            && !frame_allocator::free_contiguous_pages_tagged(
                self.user_stack.start,
                TASK_USER_STACK_PAGES,
                self.user_stack_generation,
            )
        {
            console::write_str("STACK: user stack release refused for 0x");
            console::write_hex_u64(self.user_stack.start as u64);
            console::write_str(" - leaked\r\n");
        }
    }

    /// Tears the task's address space down. `dma_revokes` carries the revoke
    /// results collected in `reap_dead_tasks` (before any teardown): a range
    /// is released only when its scan proved `safe_to_free`; otherwise the
    /// frames are deliberately leaked (see `DMA_LEAKED_FRAMES`) rather than
    /// handed back while a foreign mapping might still reference them.
    ///
    /// Quiescence precondition: `safe_to_free` is a CPU-mapping proof. It does
    /// not prove that the device stopped DMA. The kernel therefore hands a
    /// provably-unmapped range to the DMA quarantine (see
    /// [`DMA_QUARANTINE_EPOCHS`]) instead of freeing it at once, which gives
    /// an armed queue a bounded drain window; a driver that owns a device must
    /// still stop the queue (or reset it and observe completion) before it
    /// exits, and a full quarantine falls back to the leak policy.
    ///
    /// Guarded by `resources_released`/`ttbr0`: a zombie has already run this
    /// once, and calling it again with `ttbr0 == 0` would dereference zero.
    pub(super) fn release_space(&mut self, dma_revokes: &[(usize, usize, mmu::RevokeResult)]) {
        if !self.owns_space || self.resources_released || self.ttbr0 == 0 {
            return;
        }
        // Take ownership so the release logic has exactly one source of truth
        // and can also be reused by `execve` for a superseded address space.
        let resources = SpaceResources {
            ttbr0: self.ttbr0,
            space_token: self.space_token,
            private_page: self.private_page,
            private_page_generation: self.private_page_generation,
            mmap_regions: core::mem::take(&mut self.mmap_regions),
            heap_pages: core::mem::take(&mut self.heap_pages),
            dma_pages: core::mem::take(&mut self.dma_pages),
            user_image: self.user_image.take(),
            #[cfg(feature = "linux-abi")]
            linux_image: self.linux_image.take(),
        };
        self.signal_trampoline = None;
        release_space_resources(&resources, dma_revokes);
    }

    /// Drops the last executable-resource references after teardown. The
    /// zombie keeps only what `wait` needs (PID, PPID, exit status); clearing
    /// the fields makes a second teardown a no-op (see the guards above).
    pub(super) fn clear_resources(&mut self) {
        self.stack = MemoryRegion::new(0, 0);
        self.user_stack = MemoryRegion::new(0, 0);
        self.user_stack_generation = 0;
        self.private_page = 0;
        self.private_page_generation = 0;
        self.space_token = TableToken::empty();
        self.ttbr0 = 0;
        self.user_image = None;
        self.linux_image = None;
        self.heap_pages.clear();
        self.dma_pages.clear();
        self.mmap_regions.clear();
        self.mmap_bytes = 0;
        // The ledger is only meaningful while the task can allocate; a zombie
        // keeps metadata only, so drop the stale charge with the resources.
        self.mem_frames = 0;
        self.arguments = Vec::new();
        self.argv.clear();
        self.envp.clear();
        self.cwd = String::from("/");
        self.resume_state = None;
        self.waiting_on = None;
        self.context = TaskContext::zero();
        self.fp = smp::FpState::zero();
        self.resources_released = true;
    }
}
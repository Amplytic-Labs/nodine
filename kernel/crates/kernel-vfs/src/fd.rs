//! # fd — the kernel file-descriptor subsystem (P2-005-005)
//!
//! Frozen architecture: `docs/phase2/P2_005_FD_SPEC.md` (P2-005-005-003, after
//! adversarial review F — ACCEPT-WITH-CONDITIONS, all 13 conditions applied).
//!
//! Two namespaces (ABI §8.3): per-process POSIX-shaped `fd` (u32, 0/1/2 stdio,
//! first allocatable 3, lowest-free reuse) and kernel-global `OfdId` (u64,
//! monotonic per boot, never reused — the stamped slot-recycling slab, FC-7).
//! The OFD owns THE shared offset (INV-FD-1), the access mode, the append flag
//! and the inode reference; the fd table only holds OfdIds.
//!
//! Design invariants (spec cross-references inline):
//! - ONE descriptor architecture; the bridge holds no fd state (§1).
//! - Serial kernel (ABI §22): no locks; one op = one linearizable transition.
//! - The open path is allocation-free between the vfs pin and the fd install
//!   (FC-5): fd-slot and OFD-slab capacity are reserved BEFORE `vfs.open`,
//!   and the only post-pin failure (ENFILE at mint) rolls the pin back.
//! - `close` on stdio device OFDs is a no-op success (ADR-0008); teardown of
//!   a process releases their refs (FC-6 shared boot-minted triple).
//! - Offsets follow the differential-pinned state machine (§5, E-5..E-9d):
//!   explicit `pos` NEVER mutates the OFD offset — including O_APPEND writes
//!   (FC-4); `pos = -1` advances; append repositions to EOF+n only on the -1
//!   path; zero-length ops never move it.
//! - `reclaim_process` is the ONE shared teardown for exit, death and recycle
//!   (FC-2), gated on `endpoints == 0` (FC-3).

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::num::NonZeroU64;

use crate::vfs::{OpenModes, StatData, Vfs};
use crate::{Errno, VResult};

// ---------------------------------------------------------------------------
// Constants (frozen: FD_SPEC §2; ADR-0008; ABI §8.3)
// ---------------------------------------------------------------------------

/// Per-process fd ceiling. Exhaustion ⇒ EMFILE (uv −24). Host-tunable shape;
/// the default is the frozen ABI value.
pub const FD_MAX_PER_PROC: u32 = 1024;
/// Kernel-global live-OFD ceiling. Exhaustion ⇒ ENFILE (uv −23).
pub const OFD_MAX_GLOBAL: u32 = 8192;
/// Bits of OfdId used by the slot index (2^13 = 8192 = OFD_MAX_GLOBAL).
const OFD_SLOT_BITS: u32 = 13;
const OFD_SLOT_MASK: u64 = (1 << OFD_SLOT_BITS) - 1;
/// Stamp ceiling (FC-7 rider): `next_stamp` may never reach 2^51 — honest
/// ENFILE at exhaustion (practically unreachable: 71+ years at 1M mints/s).
const OFD_STAMP_MAX: u64 = 1 << 51;
/// Per-stream stdio sink capacity (FC-6/P-7): bounded FIFO; overflow drops
/// with a diagnostic counter — a serial kernel never blocks on a full pipe.
pub const STDIO_FIFO_CAP: usize = 256 * 1024;
/// Default per-process umask (VFS_SPEC §3; applied `mode & !umask` at create).
pub const FD_DEFAULT_UMASK: u16 = crate::vfs::DEFAULT_UMASK;

/// Max live processes — bounded by the ABI's MAX_ENDPOINTS (64; one endpoint
/// per process in Phase 2). The fd layer owns its own copy because
/// kernel-vfs does not depend on kernel-abi; the values are frozen together.
pub const FD_MAX_PROCESSES: u32 = 64;

// acc_and_flags packing (FD_SPEC §2): [acc:2][append:1][kind:3][spare:2]
const ACC_SHIFT: u8 = 0;
const APPEND_SHIFT: u8 = 2;
const KIND_SHIFT: u8 = 3;
const KIND_MASK: u8 = 0b111;

/// Access mode bits (frozen OpenModes::ACC_MASK values).
pub const ACC_RDONLY: u8 = 0;
pub const ACC_WRONLY: u8 = 1;
pub const ACC_RDWR: u8 = 2;

/// OFD kind (FD_SPEC §2; devices per ADR-0008).
pub const KIND_FILE: u8 = 0;
pub const KIND_DIR: u8 = 1;
pub const KIND_DEV_STDIN: u8 = 2;
pub const KIND_DEV_STDOUT: u8 = 3;
pub const KIND_DEV_STDERR: u8 = 4;

// ---------------------------------------------------------------------------
// OpenFile — the 16-byte open file description (frozen shape, VFS_SPEC §6)
// ---------------------------------------------------------------------------

/// One open file description. `offset` is THE shared offset (INV-FD-1).
#[derive(Debug, Clone, Copy)]
pub struct OpenFile {
    pub offset: u64,
    /// Pinned kernel-vfs inode idx (unused for device OFDs).
    pub ino_idx: u32,
    /// #fd-table slots referencing this OFD. Production invariant: 1 (no
    /// aliasing op in v1.0.2); `install_alias` is the test/Phase-3 seam.
    /// u16 fits: max 64 procs × 1021 allocatable fds = 65 344 < 65 535 (FC-12).
    pub refs: u16,
    pub acc_and_flags: u8,
}

impl OpenFile {
    fn new(offset: u64, ino_idx: u32, acc: u8, append: bool, kind: u8) -> OpenFile {
        OpenFile {
            offset,
            ino_idx,
            refs: 0,
            acc_and_flags: (acc << ACC_SHIFT)
                | (((append as u8)) << APPEND_SHIFT)
                | ((kind & KIND_MASK) << KIND_SHIFT),
        }
    }
    #[inline]
    pub fn acc(&self) -> u8 {
        (self.acc_and_flags >> ACC_SHIFT) & 0b11
    }
    #[inline]
    pub fn append(&self) -> bool {
        (self.acc_and_flags >> APPEND_SHIFT) & 1 != 0
    }
    #[inline]
    pub fn kind(&self) -> u8 {
        (self.acc_and_flags >> KIND_SHIFT) & KIND_MASK
    }
    #[inline]
    pub fn is_device(&self) -> bool {
        matches!(self.kind(), KIND_DEV_STDIN | KIND_DEV_STDOUT | KIND_DEV_STDERR)
    }
}

const _: () = assert!(core::mem::size_of::<OpenFile>() == 16);

// ---------------------------------------------------------------------------
// OfdTable — the stamped slot-recycling slab (FC-7)
// ---------------------------------------------------------------------------

/// Kernel-global OFD table: `id = (stamp << 13) | slot`, stamps strictly
/// increasing and never reused (monotonic ids, no ABA), `stamps[s] == 0 ⇔
/// free`, bounded 8192 slots, O(1) lookup (extract slot, compare stamp).
#[derive(Debug)]
pub struct OfdTable {
    stamps: Vec<u64>,
    slots: Vec<OpenFile>,
    free: Vec<u32>,
    next_stamp: u64,
    /// Maintained live count (== #nonzero stamps) — the ENFILE budget read.
    pub live: u32,
    /// High-water mark of `live` (diagnostic).
    pub live_high_water: u32,
}

impl OfdTable {
    fn new() -> OfdTable {
        OfdTable {
            stamps: Vec::new(),
            slots: Vec::new(),
            free: Vec::new(),
            next_stamp: 1, // stamp 0 ⇔ free slot; id 0 un-mintable
            live: 0,
            live_high_water: 0,
        }
    }

    /// Pre-flight + pre-reserve: after this call, `mint` performs NO
    /// allocation (FC-5's allocation-free mutation window). Returns the
    /// would-be capacity error early as `None` when the table is at the
    /// global cap with no free slot (mint will report ENFILE after the pin,
    /// per the frozen check order — the caller decides when to check).
    fn reserve_one(&mut self, cap: u32) -> bool {
        if !self.free.is_empty() {
            return true; // mint pops the free list — allocation-free
        }
        if self.stamps.len() >= cap as usize {
            return false; // at the ceiling with no free slot
        }
        // Growth 1.25x per frozen D1a (crate grow_125 is private; inline the
        // same arithmetic for these internal vecs).
        let len = self.stamps.len();
        let cap_now = self.stamps.capacity();
        if len == cap_now {
            let new_cap = if cap_now == 0 { 16 } else { cap_now + cap_now.div_ceil(4) };
            let want = (len + 1).max(new_cap.min(cap as usize));
            self.stamps.reserve_exact(want - len);
            let cap_now2 = self.slots.capacity();
            if len == cap_now2 {
                self.slots.reserve_exact(want - len);
            }
        } else {
            self.stamps.reserve_exact(1);
            self.slots.reserve_exact(1);
        }
        true
    }

    /// Mint an OFD under `id = (stamp << 13) | slot`. Callers must have run
    /// `reserve_one` (or accept the ENFILE). No allocation on the reserved
    /// path; appends within reserved capacity.
    fn mint(&mut self, of: OpenFile, cap: u32) -> VResult<u64> {
        if self.next_stamp >= OFD_STAMP_MAX {
            return Err(Errno::Enfile); // FC-7 rider: honest exhaustion
        }
        let slot = match self.free.pop() {
            Some(s) => s as usize,
            None => {
                if self.stamps.len() >= cap as usize {
                    return Err(Errno::Enfile);
                }
                self.stamps.push(0);
                self.slots.push(of);
                let s = self.stamps.len() - 1;
                self.stamps[s] = self.next_stamp;
                self.next_stamp += 1;
                self.live += 1;
                self.live_high_water = self.live_high_water.max(self.live);
                return Ok((self.stamps[s] << OFD_SLOT_BITS) | s as u64);
            }
        };
        debug_assert_eq!(self.stamps[slot], 0, "free-list slot must be unstamped");
        self.slots[slot] = of;
        self.stamps[slot] = self.next_stamp;
        self.next_stamp += 1;
        self.live += 1;
        self.live_high_water = self.live_high_water.max(self.live);
        Ok((self.stamps[slot] << OFD_SLOT_BITS) | slot as u64)
    }

    /// Resolve an OfdId. O(1); a stale id never matches (slot stamp differs).
    /// Test-only stamps dump.
    pub fn debug_stamps(&self) -> &[u64] {
        &self.stamps
    }

    fn get(&self, id: u64) -> Option<&OpenFile> {
        let slot = (id & OFD_SLOT_MASK) as usize;
        if slot >= self.stamps.len() {
            return None;
        }
        let stamp = id >> OFD_SLOT_BITS;
        if stamp == 0 || self.stamps[slot] != stamp {
            return None;
        }
        Some(&self.slots[slot])
    }

    fn get_mut(&mut self, id: u64) -> Option<&mut OpenFile> {
        let slot = (id & OFD_SLOT_MASK) as usize;
        if slot >= self.stamps.len() {
            return None;
        }
        let stamp = id >> OFD_SLOT_BITS;
        if stamp == 0 || self.stamps[slot] != stamp {
            return None;
        }
        Some(&mut self.slots[slot])
    }

    /// Destroy an OFD (caller checked refs == 0 and non-device). FC-7 rider:
    /// debug-assert the live-id discipline (double free / ABA detection).
    fn destroy(&mut self, id: u64) {
        let slot = (id & OFD_SLOT_MASK) as usize;
        debug_assert!(
            slot < self.stamps.len() && self.stamps[slot] == id >> OFD_SLOT_BITS,
            "destroy of a non-live OfdId"
        );
        if slot >= self.stamps.len() || self.stamps[slot] != id >> OFD_SLOT_BITS {
            return; // stale — never panic in release
        }
        self.stamps[slot] = 0;
        self.free.push(slot as u32);
        self.live -= 1;
    }
}

// ---------------------------------------------------------------------------
// FdTable — per-process, 1024-bit bitmap lowest-free from 3 (frozen §3)
// ---------------------------------------------------------------------------

/// Per-process fd table. Slots 0-2 are stdio at birth, never cleared;
/// `open_count` includes them (EMFILE at 1024 total ⇒ 1021 allocatable,
/// Linux RLIMIT_NOFILE parity — FC-6).
#[derive(Debug)]
pub struct FdTable {
    bitmap: [u64; 16],
    /// len = high-water; None ⇔ bitmap bit clear. NonZeroU64: id 0 is never
    /// minted, so the niche costs nothing (FC-7).
    slots: Vec<Option<NonZeroU64>>,
    pub open_count: u32,
    hint: u32,
}

impl FdTable {
    fn new(stdio: [u64; 3]) -> FdTable {
        let mut t = FdTable {
            bitmap: [0; 16],
            slots: Vec::new(),
            open_count: 0,
            hint: 3,
        };
        // Reserve the initial window [0, 4) so the first open() allocation is
        // covered even before its own reserve (birth itself may allocate).
        t.slots.reserve_exact(16);
        for i in 0..4 {
            let id: Option<NonZeroU64> = if i < 3 {
                Some(NonZeroU64::new(stdio[i as usize]).expect("stdio OfdId must be nonzero"))
            } else {
                None
            };
            t.slots.push(id);
        }
        for i in 0..3u32 {
            t.bitmap[(i / 64) as usize] |= 1 << (i % 64);
            t.open_count += 1;
        }
        t
    }

    /// Pre-reserve capacity for `slots.len() + 1` (FC-5: allocation-free
    /// install). True when the window is already covered.
    fn reserve_one(&mut self) -> bool {
        if self.slots.len() < self.slots.capacity() {
            return true;
        }
        if self.slots.len() >= FD_MAX_PER_PROC as usize {
            return false;
        }
        self.slots.reserve_exact(1);
        true
    }

    /// Lowest-free install ≥ 3. Caller checked EMFILE; debug-assert it here.
    fn alloc(&mut self, id: u64) -> Option<u32> {
        debug_assert!(self.open_count < FD_MAX_PER_PROC, "EMFILE pre-check missing");
        let start = self.hint.max(3) as usize;
        if start < self.slots.len() {
            // scan existing window [start, len)
            let found = (start..self.slots.len())
                .find(|&b| self.bitmap[b / 64] & (1 << (b % 64)) == 0);
            if let Some(b) = found {
                self.install(b as u32, id);
                return Some(b as u32);
            }
        }
        // extend to a new high-water slot (capacity reserved)
        if self.slots.len() >= FD_MAX_PER_PROC as usize {
            return None;
        }
        let b = self.slots.len() as u32;
        self.slots.push(None);
        self.install(b, id);
        Some(b)
    }

    fn install(&mut self, fd: u32, id: u64) {
        self.bitmap[(fd / 64) as usize] |= 1 << (fd % 64);
        self.slots[fd as usize] = NonZeroU64::new(id);
        self.open_count += 1;
        self.hint = fd + 1;
    }

    /// dup2-shaped explicit-slot install (test alias seam). The caller has
    /// verified the slot is free and reserved capacity; extends the window
    /// with `None` holes if `fd` is beyond the current high-water.
    fn install_at(&mut self, fd: u32, id: u64) {
        while self.slots.len() <= fd as usize {
            self.slots.push(None);
        }
        self.install(fd, id);
    }

    pub fn get(&self, fd: u32) -> Option<u64> {
        if fd as usize >= self.slots.len() {
            return None;
        }
        self.slots[fd as usize].map(|v| v.get())
    }

    /// Clear a slot (regular fds only — stdio never passes here).
    fn clear(&mut self, fd: u32) -> Option<u64> {
        let id = self.get(fd)?;
        self.bitmap[(fd / 64) as usize] &= !(1 << (fd % 64));
        self.slots[fd as usize] = None;
        self.open_count -= 1;
        if fd < self.hint {
            self.hint = fd;
        }
        Some(id)
    }

    /// Highest-ever-allocated fd + 1 (diagnostic high-water).
    pub fn high_water(&self) -> u32 {
        self.slots.len() as u32
    }

    /// Test-only slot dump.
    pub fn fds_debug(&self) -> Vec<Option<u64>> {
        self.slots.iter().map(|s| s.map(|v| v.get())).collect()
    }
}

// ---------------------------------------------------------------------------
// ProcessRecord / FdState
// ---------------------------------------------------------------------------

/// One Node process (keyed by pid — NEVER endpoint index; recycling and
/// Phase-3 multi-endpoint processes make index keying structurally wrong).
#[derive(Debug)]
pub struct ProcessRecord {
    pub pid: u32,
    /// cwd — a pinned inode (VFS_SPEC 0-7). Released at reclaim.
    pub cwd_idx: u32,
    pub umask: u16,
    /// Endpoint count (Phase 2 ≡ 1). Teardown gated on 0 (FC-3).
    pub endpoints: u32,
    pub fds: FdTable,
    /// P2-006-008-3: PER-PROCESS terminal input FIFO. The host feed fans out
    /// a copy into every live process's FIFO (nodine_stdin_feed), and fd-0
    /// reads drain the CALLING process's own copy. Root cause this fixes: a
    /// shared single FIFO let a parent's zombie stdin reader (npm's `read`
    /// package: readline closed + pause(), but the in-flight EAGAIN poll kept
    /// consuming) RACE the foreground child for every keystroke — observed
    /// live in the Expo golden flow: 'NodineExpoGolden' arrived as
    /// 'racexn' and the Enter was stolen entirely (create-expo waited at the
    /// name prompt for 20 minutes). Real ttys arbitrate one-reader-per-byte at
    /// the OS level and libuv's readStop actually detaches a paused reader; a
    /// serial kernel cannot replicate that arbitration without per-process
    // buffers, so input is REPLICATED instead (documented divergence: every
    // process sees the full terminal input stream; a non-reading process's
    // copy simply accumulates in its bounded FIFO).
    pub stdin_in: VecDeque<u8>,
}

/// Diag counters (kernel-core nodine_diag fields 27-33; FD_SPEC §8).
#[derive(Debug, Default, Clone, Copy)]
pub struct FdCounters {
    pub ofd_mints: u64,
    pub ofd_destroys: u64,
    pub stdio_out_bytes: u64,
    pub stdio_out_drops: u64,
    pub stdio_err_bytes: u64,
    pub stdio_err_drops: u64,
    pub stdin_in_bytes: u64,
    pub stdin_in_drops: u64,
}

/// Caps (host-tunable shape; defaults are the frozen ABI values). Unit tests
/// override to exercise EMFILE/ENFILE cheaply.
#[derive(Debug, Clone, Copy)]
pub struct FdCaps {
    pub fd_max: u32,
    pub ofd_max: u32,
}

impl Default for FdCaps {
    fn default() -> Self {
        FdCaps { fd_max: FD_MAX_PER_PROC, ofd_max: OFD_MAX_GLOBAL }
    }
}

/// THE kernel fd state — one instance, kalloc-allocated at boot, rooted at
/// the KH cell (kernel-core). All descriptor state lives here; the bridge
/// holds none.
pub struct FdState {
    pub ofds: OfdTable,
    procs: Vec<ProcessRecord>,
    /// Shared boot-minted stdio triple (FC-6): ids of the DevStdin /
    /// DevStdout / DevStderr OFDs; every process birth installs these same
    /// three ids at slots 0/1/2.
    pub stdio: [u64; 3],
    pub counters: FdCounters,
    pub caps: FdCaps,
    stdio_out: VecDeque<u8>,
    stdio_err: VecDeque<u8>,
    /// Stdin INPUT is per-process since P2-006-008-3 (fan-out feed; see
    /// ProcessRecord::stdin_in). The old global FIFO is gone.
    stdio_in_retired: (),
    /// Highest `procs.len()`-era fd high-water (diag): max FdTable span.
    pub fd_high_water: u32,
}

impl FdState {
    /// Boot: mint the shared stdio device triple (ENFILE baseline live = 3).
    pub fn new() -> FdState {
        let mut ofds = OfdTable::new();
        ofds.reserve_one(OFD_MAX_GLOBAL);
        let stdin = ofds
            .mint(OpenFile::new(0, 0, ACC_RDONLY, false, KIND_DEV_STDIN), OFD_MAX_GLOBAL)
            .expect("boot stdio mint within the floor");
        let stdout = ofds
            .mint(OpenFile::new(0, 0, ACC_WRONLY, false, KIND_DEV_STDOUT), OFD_MAX_GLOBAL)
            .expect("boot stdio mint within the floor");
        let stderr = ofds
            .mint(OpenFile::new(0, 0, ACC_WRONLY, false, KIND_DEV_STDERR), OFD_MAX_GLOBAL)
            .expect("boot stdio mint within the floor");
        let counters = FdCounters { ofd_mints: 3, ..FdCounters::default() };
        FdState {
            ofds,
            procs: Vec::new(),
            stdio: [stdin, stdout, stderr],
            counters,
            caps: FdCaps::default(),
            stdio_out: VecDeque::new(),
            stdio_err: VecDeque::new(),
            stdio_in_retired: (),
            fd_high_water: 4,
        }
    }

    #[inline]
    fn find(&self, pid: u32) -> Option<usize> {
        self.procs.iter().position(|p| p.pid == pid)
    }

    /// Test/diagnostic iteration over process records (readonly).
    pub fn procs_iter(&self) -> impl Iterator<Item = &ProcessRecord> {
        self.procs.iter()
    }

    /// Test-only mutable access to a process record (umask overrides in
    /// T-FD-19; never used by kernel-core dispatch).
    pub fn procs_iter_mut(&mut self) -> impl Iterator<Item = &mut ProcessRecord> {
        self.procs.iter_mut()
    }

    /// Live process count (pid validation at endpoint alloc, FC-11).
    pub fn process_count(&self) -> u32 {
        self.procs.len() as u32
    }

    pub fn process_exists(&self, pid: u32) -> bool {
        self.find(pid).is_some()
    }

    /// Process birth (boot pid-1 path and OP_PROC_READY path — create-if-
    /// absent keyed by pid). Idempotent: an existing record is reused without
    /// touching stdio refs or the endpoint count (a same-pid second READY can
    /// only be a stale re-READY in Phase 2 — pid uniqueness at alloc makes a
    /// second endpoint for a live pid impossible; Phase 3 refines this with
    /// per-endpoint birth tracking). Pins the root as cwd (VFS_SPEC 0-7).
    pub fn process_birth(&mut self, vfs: &mut Vfs, pid: u32, umask: u16) -> VResult<()> {
        if pid == 0 {
            return Err(Errno::Einval); // pid 0 is the kernel namespace
        }
        if self.find(pid).is_some() {
            return Ok(()); // idempotent (T-FD-22)
        }
        if self.procs.len() as u32 >= FD_MAX_PROCESSES {
            return Err(Errno::Enfile); // process budget (≤ MAX_ENDPOINTS class)
        }
        // Pre-allocate everything BEFORE the first vfs mutation (FC-5):
        let record = ProcessRecord {
            pid,
            cwd_idx: vfs.root_idx(),
            umask,
            endpoints: 1,
            fds: FdTable::new(self.stdio),
            stdin_in: VecDeque::new(),
        };
        // stdio refs (shared triple — refs += 1 per installing process)
        for id in self.stdio {
            if let Some(ofd) = self.ofds.get_mut(id) {
                ofd.refs = ofd.refs.saturating_add(1);
            }
        }
        vfs.pin(vfs.root_idx()); // cwd pin (open_count += 1)
        self.fd_high_water = self.fd_high_water.max(record.fds.high_water());
        self.procs.push(record);
        Ok(())
    }

    /// THE shared teardown (FC-2): invoked from endpoint_drain (exit) AND
    /// nodine_endpoint_died (death) — pid read from the slot before the
    /// DEAD store in both paths. Gated on endpoints == 0 (FC-3).
    ///
    /// Order: endpoints −= 1 → (at 0 only:) walk fd slots (stdio slots:
    /// shared-triple refs −= 1; regular slots: OFD refs −= 1, destroy at 0 →
    /// vfs.close — POSIX last-close reclaim of unlinked inodes) → cwd unpin
    /// → drop the record.
    pub fn reclaim_process(&mut self, vfs: &mut Vfs, pid: u32) {
        let Some(i) = self.find(pid) else { return };
        if self.procs[i].endpoints > 1 {
            self.procs[i].endpoints -= 1;
            return; // live sibling endpoints (Phase 3) — nothing else touched
        }
        let record = self.procs.swap_remove(i);
        let mut close_list: Vec<(u64, bool, u32)> = Vec::new(); // (id, device, ino_idx)
        for slot in record.fds.slots.iter() {
            if let Some(id) = slot {
                if let Some(ofd) = self.ofds.get(id.get()) {
                    close_list.push((id.get(), ofd.is_device(), ofd.ino_idx));
                }
            }
        }
        for (id, device, ino_idx) in close_list {
            if device {
                if let Some(ofd) = self.ofds.get_mut(id) {
                    ofd.refs = ofd.refs.saturating_sub(1); // shared triple: refs−−, never destroyed
                }
                continue;
            }
            let last = {
                let Some(ofd) = self.ofds.get_mut(id) else { continue };
                if ofd.refs > 0 {
                    ofd.refs -= 1;
                }
                ofd.refs == 0
            };
            if last {
                self.ofds.destroy(id);
                self.counters.ofd_destroys += 1;
                let _ = vfs.close(ino_idx); // pin release; last-close reclaim
            }
        }
        let _ = vfs.close(record.cwd_idx); // cwd pin release (orphan refund, G13)
    }

    // -----------------------------------------------------------------
    // OP_OPEN (FD_SPEC §3 check order — frozen)
    // -----------------------------------------------------------------

    pub fn open(
        &mut self,
        vfs: &mut Vfs,
        pid: u32,
        path: &[u8],
        flags: u32,
        mode: u16,
        now: i64,
    ) -> VResult<u32> {
        let acc = (flags & OpenModes::ACC_MASK) as u8;
        let append = flags & OpenModes::APPEND != 0;
        // FD_SPEC §6 row 14: unknown flag bits ⇒ EINVAL (defense-in-depth —
        // the wire-level §19.2 check is the P2-005-006 obligation). Known
        // bits: Node's stringToFlags surface + the documented accepted-no-op
        // set (VFS_SPEC §4.2:401-405: O_NOCTTY/O_NONBLOCK/O_DSYNC/
        // O_DIRECT/O_DIRECTORY/O_NOFOLLOW/O_NOATIME/O_CLOEXEC).
        const KNOWN_FLAGS: u32 = OpenModes::ACC_MASK // 0b11
            | OpenModes::CREAT      // 1<<6
            | OpenModes::EXCL       // 1<<7
            | (1 << 8)              // O_NOCTTY
            | OpenModes::TRUNC      // 1<<9
            | OpenModes::APPEND     // 1<<10
            | (1 << 11)             // O_NONBLOCK
            | (1 << 12)             // O_DSYNC
            | (1 << 15)             // O_DIRECT
            | (1 << 16)             // O_DIRECTORY
            | (1 << 17)             // O_NOFOLLOW
            | (1 << 20)             // O_NOATIME
            | (1 << 21);            // O_CLOEXEC
        if flags & !KNOWN_FLAGS != 0 {
            return Err(Errno::Einval);
        }
        let pi = self.find(pid).ok_or(Errno::Ebadf)?;
        // (1) EMFILE pre-resolution — nothing allocated (E-5: EMFILE beats
        // ENOENT; error completion carries `path` — open-family rule).
        if self.procs[pi].fds.open_count >= self.caps.fd_max {
            return Err(Errno::Emfile);
        }
        // (2) Reserve fd-slot + OFD-slab capacity BEFORE the pin — the
        // window [vfs.open .. fd install] is allocation-free (FC-5).
        if !self.procs[pi].fds.reserve_one() {
            return Err(Errno::Emfile); // bitmap full (== fd_max slots in use)
        }
        let ofd_room = self.ofds.reserve_one(self.caps.ofd_max);
        // (3) Path resolve + flag matrix (vfs.open — §6 rows; pins the inode).
        let umask = self.procs[pi].umask;
        let start = self.procs[pi].cwd_idx;
        let idx = vfs.open(start, path, flags, mode, umask, now)?;
        // (4) ENFILE at OFD mint — rollback the pin (FC-5 rollback invariant).
        let kind = if vfs.idx_is_dir(idx) { KIND_DIR } else { KIND_FILE };
        // refs = 1 at mint: the installing fd slot (found as the REAL bug by
        // T-FD-10 — a 0-refs birth OFD made close() destroy by accident and
        // broke the alias refcount).
        let mut of = OpenFile::new(0, idx, acc, append, kind);
        of.refs = 1;
        let id = match self.ofds.mint(of, self.caps.ofd_max) {
            Ok(id) => id,
            Err(e) => {
                let _ = vfs.close(idx); // unpin — no leak on the ENFILE path
                return Err(e);
            }
        };
        if !ofd_room {
            // reserve_one reported no room pre-pin but the mint succeeded
            // (a free slot appeared) — benign; nothing to roll back.
        }
        self.counters.ofd_mints += 1;
        // (5) Install lowest-free (allocation-free; EMFILE pre-checked).
        let fd = self.procs[pi]
            .fds
            .alloc(id)
            .expect("EMFILE pre-checked + capacity reserved");
        self.fd_high_water = self.fd_high_water.max(self.procs[pi].fds.high_water());
        Ok(fd)
    }

    // -----------------------------------------------------------------
    // OP_CLOSE
    // -----------------------------------------------------------------

    pub fn close(&mut self, vfs: &mut Vfs, pid: u32, fd: u32) -> VResult<()> {
        let pi = self.find(pid).ok_or(Errno::Ebadf)?;
        let Some(id) = self.procs[pi].fds.get(fd) else {
            return Err(Errno::Ebadf); // unknown/closed/double close (POSIX)
        };
        // stdio device slots: no-op success, mapping retained (ADR-0008).
        if let Some(ofd) = self.ofds.get(id) {
            if ofd.is_device() {
                return Ok(());
            }
        }
        let Some(_id) = self.procs[pi].fds.clear(fd) else {
            return Err(Errno::Ebadf);
        };
        let last = {
            let Some(ofd) = self.ofds.get_mut(id) else {
                return Ok(()); // stale (cannot happen — table owns it)
            };
            if ofd.refs > 0 {
                ofd.refs -= 1;
            }
            ofd.refs == 0
        };
        if last {
            let ino_idx = self.ofds.get(id).map(|o| o.ino_idx).unwrap_or(0);
            self.ofds.destroy(id);
            self.counters.ofd_destroys += 1;
            let _ = vfs.close(ino_idx); // last-close reclaim of unlinked inodes
        }
        Ok(())
    }

    // -----------------------------------------------------------------
    // OP_READ (FD_SPEC §5 rows 1-3; §7 gate order)
    // -----------------------------------------------------------------

    pub fn read(
        &mut self,
        vfs: &mut Vfs,
        pid: u32,
        fd: u32,
        pos: i64,
        buf: &mut [u8],
        now: i64,
    ) -> VResult<usize> {
        let pi = self.find(pid).ok_or(Errno::Ebadf)?;
        let id = self.procs[pi].fds.get(fd).ok_or(Errno::Ebadf)?;
        // kind gate (§7): devices per §4.3; Dir ⇒ EISDIR (after mode passes —
        // a Dir OFD is always RDONLY so the mode gate fires first for writes).
        let (kind, acc) = {
            let ofd = self.ofds.get(id).ok_or(Errno::Ebadf)?;
            (ofd.kind(), ofd.acc())
        };
        match kind {
            KIND_DEV_STDIN => {
                // P2-006-007: the stdin device is now a real input source —
                // the host feeds the FIFO (nodine_stdin_feed) and reads
                // drain it. Empty ⇒ EAGAIN (the ADR-0008 Phase-2
                // "read = EOF" divergence is SUPERSEDED; the bridge's async
                // stdin stream retries on EAGAIN — documented divergence).
                // P2-006-008-3: drains THIS process's private copy — the
                // fan-out feed gives every process its own view, so a zombie
                // parent reader can no longer steal the foreground child's
                // keystrokes (see ProcessRecord::stdin_in).
                if pos >= 0 {
                    return Err(Errno::Einval); // positional read on a tty-class device (ESPIPE class)
                }
                let fifo = &mut self.procs[pi].stdin_in;
                let n = buf.len().min(fifo.len());
                for slot in buf.iter_mut().take(n) {
                    *slot = fifo.pop_front().expect("len checked");
                }
                if n == 0 {
                    return Err(Errno::Eagain);
                }
                return Ok(n);
            }
            KIND_DEV_STDOUT | KIND_DEV_STDERR => return Err(Errno::Ebadf),
            _ => {}
        }
        if acc != ACC_RDONLY && acc != ACC_RDWR {
            return Err(Errno::Ebadf); // read on WRONLY (differential M1)
        }
        if kind == KIND_DIR {
            return Err(Errno::Eisdir); // item 8
        }
        if pos < -1 {
            return Err(Errno::Einval); // §7 pos validation (FC-9 defense)
        }
        let (at, advance) = if pos < 0 {
            (self.ofds.get(id).map(|o| o.offset).unwrap_or(0), true)
        } else {
            (pos as u64, false)
        };
        let ino_idx = self.ofds.get(id).map(|o| o.ino_idx).unwrap_or(0);
        let n = vfs.read_at(ino_idx, at, buf, now)?;
        if advance {
            if let Some(ofd) = self.ofds.get_mut(id) {
                ofd.offset = ofd.offset.saturating_add(n as u64);
            }
        }
        Ok(n)
    }

    // -----------------------------------------------------------------
    // OP_WRITE (FD_SPEC §5 rows 4-9; differential-pinned offsets)
    // -----------------------------------------------------------------

    pub fn write(
        &mut self,
        vfs: &mut Vfs,
        pid: u32,
        fd: u32,
        pos: i64,
        data: &[u8],
        now: i64,
    ) -> VResult<usize> {
        let pi = self.find(pid).ok_or(Errno::Ebadf)?;
        let id = self.procs[pi].fds.get(fd).ok_or(Errno::Ebadf)?;
        let (kind, acc, append) = {
            let ofd = self.ofds.get(id).ok_or(Errno::Ebadf)?;
            (ofd.kind(), ofd.acc(), ofd.append())
        };
        // Device matrix (§4.3).
        match kind {
            KIND_DEV_STDOUT => {
                self.sink_push(true, data);
                return Ok(data.len() as usize);
            }
            KIND_DEV_STDERR => {
                self.sink_push(false, data);
                return Ok(data.len() as usize);
            }
            KIND_DEV_STDIN => return Err(Errno::Ebadf),
            _ => {}
        }
        if acc != ACC_WRONLY && acc != ACC_RDWR {
            return Err(Errno::Ebadf); // write on RDONLY (differential M2)
        }
        if kind == KIND_DIR {
            return Err(Errno::Ebadf); // Dir OFD is RDONLY; defense in depth
        }
        if pos < -1 {
            return Err(Errno::Einval);
        }
        if data.is_empty() {
            return Ok(0); // row 8: zero-length never moves the offset (E-9b)
        }
        let ino_idx = self.ofds.get(id).map(|o| o.ino_idx).unwrap_or(0);
        // Offset resolution (§5): append ⇒ land at EOF (size at write time);
        // explicit pos never mutates the OFD offset — including append (FC-4,
        // differential E-9); −1-pos append advances to size+n (E-9d).
        let (at, advance) = if append {
            let size = vfs.idx_logical_size(ino_idx)?;
            (size, pos < 0)
        } else if pos < 0 {
            (self.ofds.get(id).map(|o| o.offset).unwrap_or(0), true)
        } else {
            (pos as u64, false)
        };
        let n = vfs.write_at(ino_idx, at, data, now)?;
        if advance {
            if let Some(ofd) = self.ofds.get_mut(id) {
                ofd.offset = at.saturating_add(n as u64);
            }
        }
        Ok(n)
    }

    // -----------------------------------------------------------------
    // OP_FSTAT
    // -----------------------------------------------------------------

    pub fn fstat(&self, vfs: &Vfs, pid: u32, fd: u32) -> VResult<StatData> {
        let pi = self.find(pid).ok_or(Errno::Ebadf)?;
        let id = self.procs[pi].fds.get(fd).ok_or(Errno::Ebadf)?;
        let ofd = self.ofds.get(id).ok_or(Errno::Ebadf)?;
        match ofd.kind() {
            KIND_DEV_STDIN | KIND_DEV_STDOUT | KIND_DEV_STDERR => {
                // Synthetic device stat (ADR-0008; §4.3): S_IFCHR shape,
                // high-bit synthetic ino (never collides with VFS inos).
                // Provisional pending the real-browser E-6 pin (P2-005-009).
                let stream = match ofd.kind() {
                    KIND_DEV_STDIN => 1u64,
                    KIND_DEV_STDOUT => 2,
                    _ => 3,
                };
                Ok(StatData {
                    dev: 0,
                    mode: 0o020666, // S_IFCHR | rw-rw-rw-
                    nlink: 1,
                    uid: 0,
                    gid: 0,
                    rdev: 5 << 8, // (5, 0) — tty-class major
                    blksize: 4096,
                    ino: 0x8000_0000_0000_0000 | stream,
                    size: 0,
                    blocks: 0,
                    atime_sec: 0,
                    atime_nsec: 0,
                    mtime_sec: 0,
                    mtime_nsec: 0,
                    ctime_sec: 0,
                    ctime_nsec: 0,
                    birthtime_sec: 0,
                    birthtime_nsec: 0,
                })
            }
            _ => vfs.stat_idx(ofd.ino_idx),
        }
    }

    // -----------------------------------------------------------------
    // OP_FTRUNCATE (ABI §7.3 0x010C — "P2-005 optional", exercised by the
    // P2-005-006 dispatch batch). Linux semantics (differential F1):
    // unknown fd ⇒ EBADF; non-regular file (device/dir) ⇒ EINVAL; fd not
    // open for writing ⇒ EINVAL (readonly regular file); offsets of open
    // OFDs are unmoved by the truncation (POSIX — vfs.ftruncate_idx).
    // -----------------------------------------------------------------

    pub fn ftruncate(&mut self, vfs: &mut Vfs, pid: u32, fd: u32, len: u64, now: i64) -> VResult<()> {
        let pi = self.find(pid).ok_or(Errno::Ebadf)?;
        let id = self.procs[pi].fds.get(fd).ok_or(Errno::Ebadf)?;
        let (kind, acc) = {
            let ofd = self.ofds.get(id).ok_or(Errno::Ebadf)?;
            (ofd.kind(), ofd.acc())
        };
        if kind != KIND_FILE {
            return Err(Errno::Einval); // devices + dirs are not truncatable (Linux EINVAL)
        }
        if acc != ACC_WRONLY && acc != ACC_RDWR {
            return Err(Errno::Einval); // differential F1: readonly fd ⇒ EINVAL
        }
        let ino_idx = self.ofds.get(id).map(|o| o.ino_idx).unwrap_or(0);
        vfs.ftruncate_idx(ino_idx, len, now)
    }

    // -----------------------------------------------------------------
    // dispatch helpers (P2-005-006): the ABI layer's path-op starting point
    // -----------------------------------------------------------------

    /// The process's (cwd inode idx, umask). None ⇒ no ProcessRecord for the
    /// pid (the dispatcher maps that to EBADF — an endpoint without a birth
    // cannot name paths).
    pub fn cwd_umask_of(&self, pid: u32) -> Option<(u32, u16)> {
        self.find(pid).map(|pi| (self.procs[pi].cwd_idx, self.procs[pi].umask))
    }

    // -----------------------------------------------------------------
    // OP_PROC_CHDIR (P2-006-007; ABI 0x0015 — process range, previously
    // unallocated). Resolve-follow-final, must land on a directory; the
    // pin swap keeps the cwd-pin invariant (VFS_SPEC 0-7: released at
    // reclaim).
    // -----------------------------------------------------------------

    pub fn chdir(&mut self, vfs: &mut Vfs, pid: u32, path: &[u8]) -> VResult<()> {
        let pi = self.find(pid).ok_or(Errno::Ebadf)?;
        let start = self.procs[pi].cwd_idx;
        let idx = vfs.resolve(start, path, true)?;
        if !vfs.idx_is_dir(idx) {
            return Err(Errno::Enotdir); // Linux chdir to a non-directory
        }
        if idx == start {
            return Ok(()); // same-directory chdir: no pin churn
        }
        let old = self.procs[pi].cwd_idx;
        vfs.pin(idx);
        self.procs[pi].cwd_idx = idx;
        let _ = vfs.close(old); // release the previous cwd pin
        Ok(())
    }

    // -----------------------------------------------------------------
    // stdin input path (P2-006-007): host → kernel FIFO → process reads.
    // P2-006-008-3: the feed FANS OUT — every live process gets its own copy
    // (per-process FIFO; see ProcessRecord::stdin_in). Mirror of the output
    // sinks: bounded, drop+count, never blocks.
    // -----------------------------------------------------------------

    pub fn stdin_feed(&mut self, data: &[u8]) {
        if self.procs.is_empty() {
            return;
        }
        // Per-process bounded copy; the global counter tracks the count
        // delivered to EVERY process (min across copies — the guaranteed
        // delivery figure; a full single FIFO still counts its accepted
        // prefix, matching the single-process bounded-drop semantics).
        let mut min_take = data.len();
        for p in self.procs.iter_mut() {
            let room = STDIO_FIFO_CAP.saturating_sub(p.stdin_in.len());
            let take = room.min(data.len());
            for &b in &data[..take] {
                p.stdin_in.push_back(b);
            }
            if data.len() - take > 0 {
                self.counters.stdin_in_drops += (data.len() - take) as u64;
            }
            min_take = min_take.min(take);
        }
        self.counters.stdin_in_bytes += min_take as u64;
    }

    /// Bytes currently buffered in the stdin FIFOs (diagnostic: sum over all
    /// processes' private copies).
    pub fn stdin_len(&self) -> usize {
        self.procs.iter().map(|p| p.stdin_in.len()).sum()
    }

    // -----------------------------------------------------------------
    // stdio sinks (§4.3: bounded FIFO, drop+count — never block, never fail)
    // -----------------------------------------------------------------

    fn sink_push(&mut self, out: bool, data: &[u8]) {
        let (fifo, bytes_ctr, drops_ctr) = if out {
            (&mut self.stdio_out, &mut self.counters.stdio_out_bytes, &mut self.counters.stdio_out_drops)
        } else {
            (&mut self.stdio_err, &mut self.counters.stdio_err_bytes, &mut self.counters.stdio_err_drops)
        };
        let room = STDIO_FIFO_CAP.saturating_sub(fifo.len());
        let take = room.min(data.len());
        for &b in &data[..take] {
            fifo.push_back(b);
        }
        *bytes_ctr += take as u64;
        let dropped = data.len() - take;
        if dropped > 0 {
            *drops_ctr += dropped as u64;
        }
    }

    /// Host drain: copy up to `out.len()` bytes from the front of the stream
    /// FIFO (0 = stdout, 1 = stderr), removing them. Returns bytes moved.
    pub fn stdio_drain(&mut self, stream: u32, out: &mut [u8]) -> usize {
        let fifo = if stream == 0 { &mut self.stdio_out } else { &mut self.stdio_err };
        let n = out.len().min(fifo.len());
        for slot in out.iter_mut().take(n) {
            *slot = fifo.pop_front().expect("len checked");
        }
        n
    }

    pub fn stdio_len(&self, stream: u32) -> usize {
        if stream == 0 {
            self.stdio_out.len()
        } else {
            self.stdio_err.len()
        }
    }

    // -----------------------------------------------------------------
    // test hooks (T-FD-10; the Phase-3 dup/transfer seam)
    // -----------------------------------------------------------------

    /// Install an alias: `fd_dst` (free, ≥ 3) references the SAME OFD as
    /// `fd_src` (refs += 1, open_count += 1) — dup2-shaped: the destination
    /// slot is EXPLICIT, not lowest-free. Test-only surface — no v1.0.2 op
    /// produces aliases; Phase-3 dup lands on exactly this machinery.
    pub fn install_alias(&mut self, pid: u32, fd_src: u32, fd_dst: u32) -> VResult<()> {
        let pi = self.find(pid).ok_or(Errno::Ebadf)?;
        if fd_dst < 3 || fd_dst >= self.caps.fd_max {
            return Err(Errno::Einval);
        }
        let id = self.procs[pi].fds.get(fd_src).ok_or(Errno::Ebadf)?;
        if self.procs[pi].fds.get(fd_dst).is_some() {
            return Err(Errno::Eexist);
        }
        if self.procs[pi].fds.open_count >= self.caps.fd_max {
            return Err(Errno::Emfile);
        }
        if !self.procs[pi].fds.reserve_one() {
            return Err(Errno::Emfile);
        }
        {
            let ofd = self.ofds.get_mut(id).ok_or(Errno::Ebadf)?;
            if ofd.refs == u16::MAX {
                return Err(Errno::Emfile); // saturation guard (FC-12)
            }
            ofd.refs += 1;
        }
        // dup2 shape: install at the explicit destination slot
        self.procs[pi].fds.install_at(fd_dst, id);
        Ok(())
    }

    /// Diagnostic snapshot (nodine_diag fields 27-33; FD_SPEC §8).
    pub fn diag(&self, field: u32) -> i64 {
        match field {
            27 => self.ofds.live as i64,
            28 => self.counters.ofd_mints as i64,
            29 => self.counters.ofd_destroys as i64,
            30 => self.counters.stdio_out_bytes as i64,
            31 => self.counters.stdio_out_drops as i64,
            32 => self.counters.stdio_err_bytes as i64,
            33 => self.counters.stdio_err_drops as i64,
            // P2-006-008: stdin FIFO observability — the Expo golden prompt
            // lost keystrokes (typed 18, app received 7); these counters
            // distinguish HOST-side feed drops from app-side read behavior.
            36 => self.counters.stdin_in_bytes as i64,
            37 => self.counters.stdin_in_drops as i64,
            _ => 0,
        }
    }
}

// ---------------------------------------------------------------------------
// unit tests: the slab/fdtable invariants (FD_SPEC FC-7 riders, §3)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn ofd(cap: u32) -> OfdTable {
        let mut t = OfdTable::new();
        t.reserve_one(cap);
        t
    }

    #[test]
    fn stamped_slab_monotonic_never_reuse_bounded() {
        let mut t = ofd(8192);
        let mut ids = Vec::new();
        for i in 0..1000u64 {
            let id = t
                .mint(OpenFile::new(i, i as u32, ACC_RDONLY, false, KIND_FILE), 8192)
                .unwrap();
            ids.push(id);
        }
        // strictly increasing over the whole mint history
        for w in ids.windows(2) {
            assert!(w[1] > w[0], "ids must be monotonic");
        }
        // churn: destroy half, re-mint — new ids still exceed ALL prior ids
        let half: Vec<u64> = ids.iter().step_by(2).copied().collect();
        let max_before = *ids.last().unwrap();
        for id in &half {
            t.destroy(*id);
        }
        assert_eq!(t.live, 1000 - half.len() as u32);
        for i in 0..half.len() as u64 {
            let id = t
                .mint(OpenFile::new(i, 0, ACC_RDONLY, false, KIND_FILE), 8192)
                .unwrap();
            assert!(id > max_before, "re-minted ids must exceed all prior ids");
            // stale ids from the destroyed half must never resolve again
            assert!(t.get(ids[(i as usize) * 2]).is_none());
        }
        // bounded: 1000 live, slab span ≤ 1000 + free-list reuse
        assert!(t.stamps.len() <= 1000);
        assert_eq!(t.live, 1000);
    }

    #[test]
    fn stamped_slab_id_zero_and_lookup() {
        let mut t = ofd(8192);
        let id = t.mint(OpenFile::new(0, 7, ACC_RDWR, true, KIND_FILE), 8192).unwrap();
        assert_ne!(id, 0, "id 0 is reserved (stamp starts at 1)");
        assert_eq!(t.get(id).unwrap().ino_idx, 7);
        assert!(t.get(id + 1).is_none(), "wrong slot stamp must not match");
        assert!(t.get(id << 1).is_none());
        assert!(t.get(0).is_none());
        assert!(t.get(1).is_none());
        // slot index within 13 bits
        assert_eq!(id & OFD_SLOT_MASK, 0); // first slot
    }

    #[test]
    fn stamped_slab_enfile_at_cap() {
        let mut t = OfdTable::new();
        t.reserve_one(4);
        for i in 0..4u64 {
            t.mint(OpenFile::new(0, 0, ACC_RDONLY, false, KIND_FILE), 4).unwrap();
        }
        assert_eq!(
            t.mint(OpenFile::new(0, 0, ACC_RDONLY, false, KIND_FILE), 4),
            Err(Errno::Enfile)
        );
        assert_eq!(t.live, 4);
        t.destroy((1u64 << OFD_SLOT_BITS) | 0); // slot 0, stamp 1
        t.mint(OpenFile::new(0, 0, ACC_RDONLY, false, KIND_FILE), 4).unwrap(); // reuses slot 0
        assert_eq!(t.live, 4);
    }

    #[test]
    fn fdtable_lowest_free_and_stdio_immortal_slots() {
        let mut t = FdTable::new([100 << 13 | 1, 100 << 13 | 2, 100 << 13 | 3]);
        assert_eq!(t.get(0), Some(100 << 13 | 1));
        assert_eq!(t.open_count, 3);
        // first alloc ≥ 3
        let a = t.alloc(500).unwrap();
        assert_eq!(a, 3);
        let b = t.alloc(501).unwrap();
        assert_eq!(b, 4);
        t.clear(3);
        let c = t.alloc(502).unwrap();
        assert_eq!(c, 3, "lowest-free reuse (fd 3 returns)");
        t.clear(3);
        t.clear(4);
        let d = t.alloc(503).unwrap();
        assert_eq!(d, 3, "lowest-free across multiple clears");
        assert_eq!(t.open_count, 4); // stdio + one
    }

    #[test]
    fn openfile_packing_roundtrip() {
        let of = OpenFile::new(0xdead_beef, 42, ACC_RDWR, true, KIND_DEV_STDERR);
        assert_eq!(of.acc(), ACC_RDWR);
        assert!(of.append());
        assert_eq!(of.kind(), KIND_DEV_STDERR);
        assert!(of.is_device());
        let of2 = OpenFile::new(0, 1, ACC_RDONLY, false, KIND_FILE);
        assert_eq!(of2.kind(), KIND_FILE);
        assert!(!of2.is_device());
    }

    // ---- OP_FTRUNCATE (P2-005-006 dispatch batch) ----

    fn ft_boot() -> (FdState, Vfs) {
        let mut vfs = Vfs::new(1_000_000_000);
        (FdState::new(), vfs)
    }

    #[test]
    fn ftruncate_semantics_linux_matrix() {
        let (mut st, mut vfs) = ft_boot();
        st.process_birth(&mut vfs, 1, 0o022).unwrap();
        let fd = st
            .open(&mut vfs, 1, b"/f", OpenModes::RDWR | OpenModes::CREAT, 0o666, 0)
            .unwrap();
        st.write(&mut vfs, 1, fd, -1, b"0123456789", 0).unwrap();
        // shrink
        st.ftruncate(&mut vfs, 1, fd, 4, 0).unwrap();
        assert_eq!(st.fstat(&vfs, 1, fd).unwrap().size, 4);
        // grow zero-fills (read POSITIONALLY - the OFD offset is still 10)
        st.ftruncate(&mut vfs, 1, fd, 8, 0).unwrap();
        let mut buf = vec![0xFFu8; 8];
        assert_eq!(st.read(&mut vfs, 1, fd, 0, &mut buf, 0).unwrap(), 8);
        assert_eq!(&buf, b"0123\x00\x00\x00\x00");
        // open OFD offset unmoved by truncation (POSIX): the sequential
        // offset is 10, so the next sequential write lands past the content.
        st.write(&mut vfs, 1, fd, -1, b"Z", 0).unwrap();
        let mut b2 = vec![0u8; 11];
        assert_eq!(st.read(&mut vfs, 1, fd, 0, &mut b2, 0).unwrap(), 11);
        assert_eq!(&b2, b"0123\x00\x00\x00\x00\x00\x00Z");
        // readonly fd ⇒ EINVAL (differential F1)
        let ro = st.open(&mut vfs, 1, b"/f", OpenModes::RDONLY, 0, 0).unwrap();
        assert_eq!(st.ftruncate(&mut vfs, 1, ro, 0, 0), Err(Errno::Einval));
        // unknown fd ⇒ EBADF
        assert_eq!(st.ftruncate(&mut vfs, 1, 99, 0, 0), Err(Errno::Ebadf));
        // device fd ⇒ EINVAL
        assert_eq!(st.ftruncate(&mut vfs, 1, 1, 0, 0), Err(Errno::Einval));
        // directory fd ⇒ EINVAL (Linux parity)
        let dfd = st.open(&mut vfs, 1, b"/", OpenModes::RDONLY, 0, 0).unwrap();
        assert_eq!(st.ftruncate(&mut vfs, 1, dfd, 0, 0), Err(Errno::Einval));
    }

    #[test]
    fn cwd_umask_of_maps_records() {
        let (mut st, mut vfs) = ft_boot();
        assert!(st.cwd_umask_of(1).is_none(), "no birth yet");
        st.process_birth(&mut vfs, 7, 0o027).unwrap();
        let (cwd, umask) = st.cwd_umask_of(7).unwrap();
        assert_eq!(umask, 0o027);
        assert_eq!(cwd, vfs.root_idx(), "cwd pins the root inode");
        assert!(st.cwd_umask_of(8).is_none());
    }
}

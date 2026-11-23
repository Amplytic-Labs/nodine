//! # nodine kernel-core — P2-004 shared-memory transport completion
//!
//! The Rust/WASM kernel of Nodine, compiled to `wasm32-unknown-unknown` and
//! instantiated in its own Dedicated Worker (the kernel worker) with the KSM —
//! a shared `WebAssembly.Memory` — imported as linear memory. Linear-memory
//! offset 0 is the KSM base (`KERNEL_ABI.md` v1.0.2 §5.2).
//!
//! P2-004 scope (directive §5): the complete shared-memory transport —
//! async request/completion ring (§12), bounded queues + backpressure + EAGAIN
//! (§17), cancellation (§18/§29), multi-endpoint concurrency, RX byte
//! accounting (§9.3), heartbeat fairness, worker death + clean shutdown
//! (§21), and permanent regression protection for the synchronous path
//! (§11).
//!
//! P2-005-004b: the kernel heap allocator (`#[global_allocator]` over the
//! KSM remainder — B D5a) and the VFS core (`kernel-vfs`) are booted here,
//! with the Phase-2 mount layout. The fs ABI OPS still return
//! `NODINE_ENOOP` until the P2-005-006 dispatch batch; `KERNEL_CAP_PERSIST`
//! is cleared (AV-12 — the frozen ABI §20.1 table lists bit 3 = 0 for
//! v1.0.2).
//!
//! Execution model (KERNEL_ABI §10): the kernel worker = this wasm module +
//! a JS driver. The driver owns all platform waits and wakes; the kernel never
//! waits or busy-loops. `nodine_pump(now_ms, op_budget)` is bounded per call
//! (op budget) so the driver can keep the §5.3 heartbeat fresh even under
//! full 64×4096 saturation (P2_004_FAILURE_SEMANTICS.md F-E1); the driver
//! re-pumps while `nodine_pending()` reports work.
//!
//! Memory discipline (load-bearing, P2-004 audit):
//! - NO Rust statics/BSS anywhere: an active data segment would be placed by
//!   wasm-ld at linear offset 0 and clobber the KCTL at instantiation. All
//!   kernel-private state lives in the kernel heap at explicit offsets
//!   (KERNEL_HEAP_BASE, §5.6), verified by `scripts/check-wasm-layout.sh`.
//! - The module's own [stack][data] block is relocated into the kernel heap
//!   by `-zstack-size=0x0508_0000` (stack top; `.cargo/config.toml`).
//!
//! One canonical implementation. No fallback. A missing capability is an
//! explicit error, never a degraded mode.

#![no_std]

use core::alloc::{GlobalAlloc, Layout as AllocLayout};
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use kernel_abi::*;
use kernel_vfs::fd::FdState;
use kernel_vfs::kalloc::Kalloc;
use kernel_vfs::Vfs;

// The VFS core is `alloc`-based; the kernel heap allocator below serves it.
extern crate alloc;
use alloc::alloc::alloc as rust_raw_alloc;

// ---------------------------------------------------------------------------
// helpers — atomic access at absolute KSM offsets (linear memory = the KSM)
// ---------------------------------------------------------------------------

/// # Safety
/// `off` must be within the linear memory and 4-byte aligned.
#[inline]
unsafe fn au32(off: u32) -> &'static AtomicU32 {
    AtomicU32::from_ptr(off as *mut u32)
}

/// # Safety
/// `off` must be within the linear memory and 8-byte aligned.
#[inline]
unsafe fn au64(off: u32) -> &'static AtomicU64 {
    AtomicU64::from_ptr(off as *mut u64)
}

/// # Safety
/// `off + len` must be within the linear memory.
#[inline]
unsafe fn write_bytes(off: u32, bytes: &[u8]) {
    core::ptr::copy_nonoverlapping(bytes.as_ptr(), off as *mut u8, bytes.len());
}

/// # Safety
/// `off + 8` must be within the linear memory.
#[inline]
unsafe fn write_u64(off: u32, v: u64) {
    (off as *mut u64).write_unaligned(v);
}

/// # Safety
/// `off + 4` must be within the linear memory.
#[inline]
unsafe fn write_u32(off: u32, v: u32) {
    (off as *mut u32).write_unaligned(v);
}

/// # Safety
/// `off` must be within the linear memory.
#[inline]
unsafe fn write_u8(off: u32, v: u8) {
    (off as *mut u8).write(v);
}

/// # Safety
/// `off + 4` must be within the linear memory.
#[inline]
unsafe fn read_u32(off: u32) -> u32 {
    (off as *const u32).read_unaligned()
}

/// # Safety
/// `off + 8` must be within the linear memory.
#[inline]
unsafe fn read_u64(off: u32) -> u64 {
    (off as *const u64).read_unaligned()
}

/// Fill `len` bytes at `off` with `fill` (len ≤ 64 in hot paths; wrapping use
/// only via `fill_area`).
/// # Safety
/// `off + len` must be within the linear memory.
#[inline]
unsafe fn fill_bytes(off: u32, len: u32, fill: u8) {
    let p = off as *mut u8;
    let mut i = 0usize;
    while i < len as usize {
        *p.add(i) = fill;
        i += 1;
    }
}

// KCTL field offsets (ControlBlock, ABI §24 §8 — repr(C)).
const KCTL_ABI_VERSION: u32 = 0x08;
const KCTL_GENERATION: u32 = 0x0C;
const KCTL_KERNEL_STATE: u32 = 0x10;
const KCTL_KERNEL_CAPS: u32 = 0x14;
const KCTL_MAX_ENDPOINTS: u32 = 0x18;
const KCTL_ENDPOINT_STRIDE: u32 = 0x1C;
const KCTL_TRANSPORT_BASE: u32 = 0x20;
const KCTL_KERNEL_HEAP_BASE: u32 = 0x24;
const KCTL_HEARTBEAT: u32 = 0x28;
const KCTL_DOORBELL_OFFSET: u32 = 0x30;
/// Stdin feed scratch (P2-006-007, ABI v1.0.3 layout addition): 1 KiB
/// inside the reserved doorbell region, at a fixed offset — the driver
/// writes terminal input bytes here and calls `nodine_stdin_feed`.
/// Reserved-region layout is documented in the KERNEL_ABI §5.2 delta.
const STDIN_FEED_SCRATCH_OFF: u32 = 0x2100;
const STDIN_FEED_SCRATCH_CAP: u32 = 1024;
const KCTL_PAGES_CURRENT: u32 = 0x34;
const KCTL_PAGES_MAX: u32 = 0x38;

// Endpoint-table slot field offsets (EndpointSlot, 64 B each).
const EPS_MAGIC: u32 = 0x00;
const EPS_EP_BASE: u32 = 0x08;
const EPS_PID: u32 = 0x0C;
const EPS_GENERATION: u32 = 0x10;
const EPS_STATE: u32 = 0x14;
const EPS_CAPS: u32 = 0x18;

// Live endpoint header field offsets (EndpointHeader at EP_HEADER_OFF).
const EPH_MAGIC: u32 = 0x00;
const EPH_PID: u32 = 0x08;
const EPH_GENERATION: u32 = 0x0C;
const EPH_STATE: u32 = 0x10;
const EPH_HEARTBEAT: u32 = 0x18;

// Ring header field offsets (RingHeader at EP_*_RING_HEADER_OFF).
const RH_HEAD: u32 = 0x00;
const RH_TAIL: u32 = 0x04;
const RH_EXTRA: u32 = 0x08; // submit: sr_sync_pending; completion: cr_seq

// Sync mailbox field offsets (SyncMailbox at EP_SYNC_MAILBOX_OFF).
const SM_SEQ: u32 = 0x00;
const SM_STATE: u32 = 0x04;
const SM_OP: u32 = 0x08;
const SM_REQ_ID: u32 = 0x10;
const SM_STATUS: u32 = 0x18;
const SM_KIND: u32 = 0x1C;
const SM_OFF: u32 = 0x20;
const SM_LEN: u32 = 0x24;
const SM_INLINE: u32 = 0x28;

// RequestRecord field offsets (64 B per submit-ring slot).
const REQ_REQ_ID: u32 = 0x00;
const REQ_OP: u32 = 0x08;
const REQ_FLAGS: u32 = 0x0C;
const REQ_TX_OFF: u32 = 0x10;
const REQ_TX_LEN: u32 = 0x14;
const REQ_A0: u32 = 0x18;
const REQ_A1: u32 = 0x20;
const REQ_A2: u32 = 0x28;
const REQ_A3: u32 = 0x30;
const REQ_A4: u32 = 0x38;

// CompletionRecord field offsets (64 B per completion-ring slot).
const CREC_REQ_ID: u32 = 0x00;
const CREC_OP: u32 = 0x08;
const CREC_STATUS: u32 = 0x0C;
const CREC_KIND: u32 = 0x10;
const CREC_LEN: u32 = 0x14;
const CREC_OFF: u32 = 0x18;
const CREC_CFLAGS: u32 = 0x1C;
const CREC_INLINE: u32 = 0x20;

#[inline]
fn ep_ring_slot(base: u32, slots_off: u32, idx: u32) -> u32 {
    base + slots_off + (idx & RING_INDEX_MASK) * 64
}

// ---------------------------------------------------------------------------
// kernel-private heap layout (P2_004_CONCURRENCY.md §4 — NOT ABI surface)
//
//   [0x0506_0000, 0x0508_0000)  wasm-ld module stack (top 0x0508_0000 —
//                                KERNEL_HEAP_BASE + 128 KiB, §5.6)
//   [0x0508_0000, __heap_base)   module .rodata (panic locations etc.) —
//                                must end BELOW the KH header; the +64 KiB
//                                gap [module data, 0x0509_0040) is the
//                                headroom (P2-005-004b: linking `alloc` +
//                                `kernel-vfs` grew .rodata to ~14 KB, which
//                                the P2-004 64 B reservation could never
//                                hold — the KH header/epdir writes were
//                                clobbering the boot prefix strings;
//                                scripts/check-wasm-layout.sh now gates on
//                                __heap_base ≤ 0x0509_0040)
//   0x0509_0040                  KH header (64 B)
//   0x0509_0080                  endpoint directory: 64 × 32 B
//   0x0509_0880                  per-endpoint blocks, 0x4480 B each (lazy,
//                                FIXED slot i = base + i*0x4480 — P2-005-004b)
//   0x051A_2880                  transport-probe scratch (4096 B)
//   0x051A_3880                  Kalloc region [base, KH_MEM_END) — the kernel
//                                heap allocator (grows with the KSM)
//
// P2-005-004b layout notes (KH_LAYOUT_VERSION 1 → 2):
// - All kernel-heap structures moved +64 KiB (KH header 0x0508_0040 →
//   0x0509_0040) to clear the enlarged module footprint (see above).
// - Endpoint blocks moved from bump allocation to their fixed slots.
// - KH header cells +0x30 (kalloc region base, 0 = uninitialized) and +0x34
//   (VFS instance offset, 0 = not booted) — the two cells were reserved
//   free in the v1 header.
//
// Per-endpoint block (0x4480 = 17,536 B):
//   +0x0000 u64 magic            +0x0008 u32 flags      +0x000C pad
//   +0x0010 consumed[512]        +0x0210 cancel_pending[512]
//   +0x0410 lens[4096] u32       (RX ledger spans, per completion-ring slot)
//   +0x4410 u32 rx_tail          +0x4414 u32 ledger_head
//   +0x4418 u32 unretired        +0x441C u32 defer_passes
//   +0x4420 u32 last_cr_head     +0x4424 pad
//   +0x4428 u64 executing_id     +0x4430 u32 executing_slot
//   +0x4434 u32 executing_tail   +0x4438 u64 stats_completed
//   +0x4440 u64 stats_canceled   +0x4448 u64 stats_eagain
//   +0x4450 u64 stats_eshutdown  +0x4458 u64 canary
//   +0x4460 pad to 0x4480
// ---------------------------------------------------------------------------

const KH_MAGIC: u64 = 0x3130_484B_4152_4143; // "CARAKH01" LE
/// KH header base — kernel-private. Above the stack top (0x0508_0000) + the
/// module-data headroom (see the layout comment above); the layout gate
/// asserts `__heap_base` (the module footprint end) stays below this.
const KH_BASE: u32 = 0x0509_0040;
const KH_VERSION_OFF: u32 = KH_BASE + 0x08;
const KH_FLAGS_OFF: u32 = KH_BASE + 0x0C;
const KH_FLAG_CORRUPT: u32 = 1 << 0;
const KH_FLAG_LEDGER_FAULT: u32 = 1 << 1;
const KH_FLAG_RING_FAULT: u32 = 1 << 2; // hostile/buggy producer overcapacity (diag)
const KH_BLOCKS_NEXT_OFF: u32 = KH_BASE + 0x10; // v1 bump frontier — UNUSED in v2 (kept: slot must read 0)
const KH_MEM_END_OFF: u32 = KH_BASE + 0x14;
const KH_PENDING_OFF: u32 = KH_BASE + 0x18;
const KH_SYNC_MASK_OFF: u32 = KH_BASE + 0x28; // per-pass sync-activity mask
const KH_CANARY_OFF: u32 = KH_BASE + 0x20;
const KH_CANARY: u64 = 0xC0FF_42D1_5EA5_ED01;
/// Kalloc region base (0 = allocator not initialized). P2-005-004b cell
/// (the v1 header left +0x30..+0x40 free).
const KH_KALLOC_OFF: u32 = KH_BASE + 0x30;
/// VFS instance offset in the kalloc region (0 = not booted).
const KH_VFS_OFF: u32 = KH_BASE + 0x34;
/// P2-005-005 (KH_LAYOUT_VERSION 3): the FD subsystem instance (`FdState` —
/// per-process fd tables + the kernel-global OFD slab; FD_SPEC §2). 0 = not
/// booted.
const KH_FDSTATE_OFF: u32 = KH_BASE + 0x38;
const KH_LAYOUT_VERSION: u32 = 3;

const EPDIR_BASE: u32 = KH_BASE + 0x40;
const EPDIR_STRIDE: u32 = 32;
const EPD_BLOCK_OFF: u32 = 0x00;
const EPD_FLAGS_OFF: u32 = 0x04;
const EPD_FLAG_ALLOCATED: u32 = 1 << 0;
const EPD_FLAG_SUSPECT: u32 = 1 << 1;
const EPD_GEN_OFF: u32 = 0x08;
const EPD_DEATHS_OFF: u32 = 0x10;

// Fixed per-endpoint block slots: [KH_BLOCKS_BASE, KH_BLOCKS_BASE +
// MAX_ENDPOINTS*BK_SIZE). 64 × 0x4480 = 0x112000.
const KH_BLOCKS_BASE: u32 = EPDIR_BASE + MAX_ENDPOINTS * EPDIR_STRIDE;
const BK_SIZE: u32 = 0x4480;
const BK_MAGIC: u64 = 0x3130_4B45_5241_4343; // "CARAEK01" LE (per-endpoint block)
const BK_MAGIC_OFF: u32 = 0x0000;
const BK_CONSUMED_OFF: u32 = 0x0010;
const BK_CANCEL_OFF: u32 = 0x0210;
const BK_LENS_OFF: u32 = 0x0410;
const BK_RX_TAIL_OFF: u32 = 0x4410;
const BK_LEDGER_HEAD_OFF: u32 = 0x4414;
const BK_UNRETIRED_OFF: u32 = 0x4418;
const BK_DEFER_OFF: u32 = 0x441C;
const BK_LAST_CRH_OFF: u32 = 0x4420;
const BK_EXEC_ID_OFF: u32 = 0x4428;
const BK_EXEC_SLOT_OFF: u32 = 0x4430;
const BK_EXEC_TAIL_OFF: u32 = 0x4434;
const BK_COMPLETED_OFF: u32 = 0x4438;
const BK_CANCELED_OFF: u32 = 0x4440;
const BK_EAGAIN_OFF: u32 = 0x4448;
const BK_ESHUTDOWN_OFF: u32 = 0x4450;
const BK_CANARY_OFF: u32 = 0x4458;
const BK_CANARY: u64 = 0xA5A5_5A5A_5A5A_A5A5;

// Scratch buffer for OP_TRANSPORT_PROBE work units (kernel heap, after the
// last possible block slot: 0x0509_0880 + 64*0x4480 = 0x051A_2880).
const KH_SCRATCH_OFF: u32 = KH_BLOCKS_BASE + MAX_ENDPOINTS * BK_SIZE; // 0x051A_2880
const KH_SCRATCH_SIZE: u32 = 4096;
/// Kalloc region base: everything above the fixed kernel map. P2-005-004b
/// moved the fixed map +64 KiB (module-footprint collision fix), shifting
/// the allocator base from the research doc's 0x0519_2880 (whose derivation
/// contained a pre-existing 4 KiB arithmetic erratum — the real v1 frontier
/// was 0x0519_3880) to 0x051A_3880; the arena shrinks by 64 KiB+4 KiB to
/// ≈941.8 MiB — immaterial to the 768 MiB budget / ~175 MiB slack math
/// (frozen spec §8, B D5a).
const KALLOC_BASE: u32 = KH_SCRATCH_OFF + KH_SCRATCH_SIZE; // 0x051A_3880
/// Boot-time minimum kalloc span (eager reservation — untouched pages cost
/// nothing; WMM §3.4c/M4c). Also the minimum growth step on exhaustion.
const KALLOC_FLOOR: u32 = 16 << 20;
const KALLOC_GROW_STEP: u32 = 16 << 20;
/// Diagnostic work-unit caps (kernel-internal, calibrated by T28/B12). The
/// caps bound the serialized kernel's worst-case single-op block far inside
/// the KERNEL_DEATH_MS (1000 ms) margin; measured ~2–5 ns per unit (64 B
/// scratch fill + re-scan check) ⇒ sync cap ≈ ≤100 ms, async ≈ ≤1 s.
pub const PROBE_WORK_MAX_ASYNC: u64 = 200_000_000;
pub const PROBE_WORK_MAX_SYNC: u64 = 20_000_000;

// bitmap helpers over kernel-heap bitmaps (512 B = 4096 bits, byte-addressed)
#[inline]
unsafe fn bit_word_ptr(base: u32, idx: u32) -> u32 {
    base + ((idx >> 3) & 0x1FF)
}
#[inline]
unsafe fn get_bit(base: u32, idx: u32) -> bool {
    let p = bit_word_ptr(base, idx) as *mut u8;
    (*p & (1 << (idx & 7))) != 0
}
#[inline]
unsafe fn set_bit(base: u32, idx: u32) {
    let p = bit_word_ptr(base, idx) as *mut u8;
    *p |= 1 << (idx & 7);
}
#[inline]
unsafe fn clear_bit(base: u32, idx: u32) {
    let p = bit_word_ptr(base, idx) as *mut u8;
    *p &= !(1 << (idx & 7));
}

/// # Safety
/// Destination must be inside the linear memory.
unsafe fn zero_bytes(off: u32, len: u32) {
    fill_bytes(off, len, 0);
}

// ---------------------------------------------------------------------------
// kernel-heap memory discipline (memory.grow on demand; §5.6,
// P2_004_CONCURRENCY §4; P2-005-004b: fixed slots + kalloc region)
// ---------------------------------------------------------------------------

/// Fixed per-endpoint block slot (documented layout above): endpoint `i`
/// owns [KH_BLOCKS_BASE + i*BK_SIZE, +BK_SIZE). Lazy = allocated (zeroed +
/// magicked) on first use, but the ADDRESS is compile-time fixed.
#[inline]
fn ep_block_addr(i: u32) -> u32 {
    KH_BLOCKS_BASE + i * BK_SIZE
}

/// Ensure the linear memory covers `end_off` (growing the shared memory in
/// place — M10 — and updating KH_MEM_END + KCTL `ksm_pages_current`), then
/// extend the kalloc region to the new end if the allocator is up. Returns
/// false when the memory cannot grow (max pages reached).
///
/// # Safety
/// Must run on the kernel (single) thread; `end_off` ≤ KSM maximum.
unsafe fn ensure_mapped(end_off: u32) -> bool {
    let mut mem_end = read_u32(KH_MEM_END_OFF);
    if end_off > mem_end {
        let cur_pages = core::arch::wasm32::memory_size(0) as u64;
        let need_pages = ((end_off as u64) + 65_535) / 65_536;
        if need_pages > cur_pages {
            let delta = (need_pages - cur_pages) as usize;
            if core::arch::wasm32::memory_grow(0, delta) == usize::MAX {
                return false; // max memory reached — honest failure
            }
        }
        let pages_now = core::arch::wasm32::memory_size(0);
        mem_end = (pages_now as u32).saturating_mul(65_536);
        write_u32(KH_MEM_END_OFF, mem_end);
        au32(KCTL_PAGES_CURRENT).store(pages_now as u32, Ordering::SeqCst);
        // The kalloc region always spans [KALLOC_BASE, KH_MEM_END): extend
        // it to absorb the growth (stateless re-attach — safe, no aliasing).
        let base = read_u32(KH_KALLOC_OFF);
        if base != 0 {
            match Kalloc::region_size(base as *mut u8) {
                Some(sz) => {
                    if let Ok(mut k) = Kalloc::attach(base as *mut u8, sz) {
                        if unsafe { k.grow((mem_end - base) as usize) }.is_err() {
                            return false; // growth split below MIN_BLOCK — impossible for page steps
                        }
                    } else {
                        return false; // corrupted region — fail loudly, never continue
                    }
                }
                None => return false,
            }
        }
    }
    true
}

/// Re-attach the kernel heap allocator from the KH cell (None when the
/// allocator is not initialized or the region is corrupt — the latter is a
/// kernel bug: callers fail their path loudly).
///
/// # Safety
/// Kernel thread only.
unsafe fn kalloc_attach() -> Option<Kalloc> {
    let base = read_u32(KH_KALLOC_OFF);
    if base == 0 {
        return None;
    }
    let sz = Kalloc::region_size(base as *mut u8)?;
    Kalloc::attach(base as *mut u8, sz).ok()
}

/// The VFS instance (None before boot / after a corrupt cell).
///
/// # Safety
/// Kernel thread only; the returned reference is never handed out twice in
/// overlapping scopes (single-threaded kernel — no reentrancy).
unsafe fn vfs_mut() -> Option<&'static mut Vfs> {
    let off = read_u32(KH_VFS_OFF);
    if off == 0 {
        return None;
    }
    Some(unsafe { &mut *(off as *mut Vfs) })
}

/// The FD subsystem instance (None before boot / after a corrupt cell).
///
/// # Safety
/// Kernel thread only; same single-threaded discipline as `vfs_mut`.
unsafe fn fdstate_mut() -> Option<&'static mut FdState> {
    let off = read_u32(KH_FDSTATE_OFF);
    if off == 0 {
        return None;
    }
    Some(unsafe { &mut *(off as *mut FdState) })
}

/// FC-2: the ONE shared process-reclaim helper — invoked from
/// `endpoint_drain` (clean exit) AND `nodine_endpoint_died` (worker
/// death), with the pid read from the slot BEFORE the DEAD store. No-ops
/// when the FD subsystem is not booted or the pid has no record.
///
/// # Safety
/// Kernel thread only.
unsafe fn fd_reclaim_process(ep_index: u32) {
    let slot = ENDPOINT_TABLE_OFFSET + ep_index * 64;
    // pid BEFORE the state store (the slot is not cleared until recycle,
    // but the ordering rule is frozen — read-then-store, never after).
    let pid = au32(slot + EPS_PID).load(Ordering::SeqCst);
    let (Some(fd), Some(vfs)) = (unsafe { fdstate_mut() }, unsafe { vfs_mut() }) else {
        return;
    };
    fd.reclaim_process(vfs, pid);
}

/// Raw alloc through the kalloc region; on exhaustion grows the KSM by at
/// least `KALLOC_GROW_STEP` (eager reservation — untouched pages are free)
/// and retries once. Null return = out of memory (trap-on-OOM discipline:
/// the VFS pre-flight budgets make this unreachable for VFS traffic — spec
/// §8/D5c — so a null here is a kernel bug, surfacing as an alloc-error
/// abort).
///
/// # Safety
/// Kernel thread only.
unsafe fn ksm_raw_alloc(size: usize, align: usize) -> *mut u8 {
    let base = read_u32(KH_KALLOC_OFF);
    if base == 0 {
        return core::ptr::null_mut(); // pre-init: nothing may allocate
    }
    let mut k = match unsafe { kalloc_attach() } {
        Some(k) => k,
        None => return core::ptr::null_mut(),
    };
    if let Some(p) = unsafe { k.alloc_aligned(size, align) } {
        return p;
    }
    // Exhausted: grow to max(current region + request + 12, +GROW_STEP).
    let cur_end = (base as u64) + Kalloc::region_size(base as *mut u8).unwrap_or(0) as u64;
    let want = cur_end
        .saturating_add(size as u64 + align as u64 + 12)
        .max(cur_end + KALLOC_GROW_STEP as u64);
    if want > u32::MAX as u64 {
        return core::ptr::null_mut();
    }
    if !unsafe { ensure_mapped(want as u32) } {
        return core::ptr::null_mut();
    }
    match unsafe { kalloc_attach() } {
        Some(mut k2) => unsafe { k2.alloc_aligned(size, align) }.unwrap_or(core::ptr::null_mut()),
        None => core::ptr::null_mut(),
    }
}

// ---------------------------------------------------------------------------
// #[global_allocator] — the KSM heap (P2-005-004b, B D5a)
//
// The `Kalloc` state lives inside the region itself; the KH cell holds the
// base. The static below is a ZERO-SIZED unit struct: no data segment is
// emitted (the layout gate re-verifies this after every build).
// ---------------------------------------------------------------------------

struct KsmGlobalAlloc;

unsafe impl GlobalAlloc for KsmGlobalAlloc {
    unsafe fn alloc(&self, layout: AllocLayout) -> *mut u8 {
        unsafe { ksm_raw_alloc(layout.size(), layout.align()) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: AllocLayout) {
        let base = read_u32(KH_KALLOC_OFF);
        if base == 0 || ptr.is_null() {
            return; // pre-init/free of null — nothing to do
        }
        if let Some(mut k) = unsafe { kalloc_attach() } {
            if layout.align() > 8 {
                unsafe { k.dealloc_aligned(ptr) };
            } else {
                unsafe { k.dealloc(ptr) };
            }
        }
        // A failed re-attach on dealloc is a corrupted region: the block
        // stays leaked rather than risking a wild free (canary checks trap
        // loudly at the next integrity pass).
    }
}

#[global_allocator]
static KSM_GLOBAL_ALLOC: KsmGlobalAlloc = KsmGlobalAlloc;

/// Kernel-private integrity check (T65/T66): KH + per-endpoint canaries.
/// On corruption: mark, then trap — a corrupted kernel heap is a kernel bug
/// (or a hostile write past the trust boundary, §19.4) and must fail loudly,
/// never continue on corrupt state (§19.1).
unsafe fn check_canaries() -> bool {
    if read_u64(KH_CANARY_OFF) != KH_CANARY {
        return false;
    }
    let mut i: u32 = 0;
    while i < MAX_ENDPOINTS {
        let dir = EPDIR_BASE + i * EPDIR_STRIDE;
        let blk = read_u32(dir + EPD_BLOCK_OFF);
        if blk != 0 && read_u64(blk + BK_CANARY_OFF) != BK_CANARY {
            return false;
        }
        i += 1;
    }
    true
}

unsafe fn mark_corrupt() {
    let f = read_u32(KH_FLAGS_OFF);
    write_u32(KH_FLAGS_OFF, f | KH_FLAG_CORRUPT);
}

// ---------------------------------------------------------------------------
// per-endpoint block init / reset
// ---------------------------------------------------------------------------

/// Zero the dynamic state of a block (bitmaps, ledger, deferral, executing).
/// Lifetime stats and magic/canary are preserved.
unsafe fn block_reset_dynamic(blk: u32) {
    zero_bytes(blk + BK_CONSUMED_OFF, 0x820); // consumed + cancel bitmaps (0x410+0x410)
    zero_bytes(blk + BK_RX_TAIL_OFF, 0x10);   // rx_tail..last_cr_head + pad
    write_u64(blk + BK_EXEC_ID_OFF, 0);
    write_u32(blk + BK_EXEC_SLOT_OFF, 0);
    write_u32(blk + BK_EXEC_TAIL_OFF, 0);
    // lens[] zeroed too (ledger restart)
    zero_bytes(blk + BK_LENS_OFF, RING_SLOTS * 4);
}

// ---------------------------------------------------------------------------
// initialization
// ---------------------------------------------------------------------------

/// Kernel boot. Idempotent-guarded: a second call while RUNNING returns
/// `NODINE_EPROTOCOL`. Writes the KCTL once-fields SeqCst, then publishes
/// `KERNEL_STATE_RUNNING` (write discipline §15.4). Allocates endpoint 0 as
/// the first process endpoint (pid 1, SYNC|ASYNC|FS_V1) and initializes the
/// kernel-private heap directory (endpoint 0's block is allocated eagerly).
///
/// `kctl_base` MUST be 0 (the frozen ABI map puts the KCTL at KSM offset 0).
/// It exists as a RUNTIME parameter so the KCTL pointer is opaque to LLVM:
/// on stock wasm32-unknown-unknown, a compile-time-constant pointer value 0
/// is provably null, and LLVM exploits null-UB — it deleted this function's
/// stores and folded the tail into an unconditional error return (observed
/// and verified by disassembly in P2-003). A parameter value cannot be
/// folded; the driver passes 0.
#[no_mangle]
pub unsafe extern "C" fn nodine_init(generation: u32, now_ms: u64, kctl_base: u32) -> i32 {
    if kctl_base != KCTL_OFFSET {
        return NODINE_EINVAL_REQUEST; // the frozen map puts the KCTL at 0
    }
    // Linear memory must hold at least the fixed ABI map (§5.2): transport
    // pool end = TRANSPORT_BASE + MAX_ENDPOINTS*EP_STRIDE = 0x0505_0000.
    let mem_pages = core::arch::wasm32::memory_size(0);
    if (mem_pages as u64) * 65_536 < (TRANSPORT_BASE as u64)
        + (MAX_ENDPOINTS as u64) * (EP_STRIDE as u64)
    {
        return NODINE_EINVAL_REQUEST; // KSM too small for the frozen map
    }
    if au32(KCTL_KERNEL_STATE).load(Ordering::SeqCst) != KERNEL_STATE_BOOTING {
        return NODINE_EPROTOCOL; // not BOOTING (double init / stale state)
    }

    // Kernel-private heap header + directory FIRST (they live above the
    // module's own [stack][data] block — see the layout comment above).
    write_u64(KH_BASE, KH_MAGIC);
    write_u32(KH_VERSION_OFF, KH_LAYOUT_VERSION);
    write_u32(KH_FLAGS_OFF, 0);
    write_u32(KH_MEM_END_OFF, (mem_pages as u32).saturating_mul(65_536));
    write_u32(KH_PENDING_OFF, 0);
    write_u32(KH_PENDING_OFF + 4, 0);
    write_u64(KH_SYNC_MASK_OFF, 0);
    write_u64(KH_CANARY_OFF, KH_CANARY);
    write_u32(KH_BLOCKS_NEXT_OFF, 0); // v2: legacy bump frontier — unused, pinned 0
    write_u32(KH_KALLOC_OFF, 0); // v2: allocator not yet up
    write_u32(KH_VFS_OFF, 0); // v2: VFS not yet booted
    write_u32(KH_FDSTATE_OFF, 0); // v3: FD subsystem not yet booted
    let mut i: u32 = 0;
    while i < MAX_ENDPOINTS {
        let dir = EPDIR_BASE + i * EPDIR_STRIDE;
        write_u32(dir + EPD_BLOCK_OFF, 0);
        write_u32(dir + EPD_FLAGS_OFF, 0);
        write_u32(dir + EPD_GEN_OFF, 0);
        write_u64(dir + EPD_DEATHS_OFF, 0);
        i += 1;
    }

    // KCTL once-fields, then the state publish (§15.4). The magic store goes
    // through the runtime-parameter base pointer (opaque; see doc comment).
    // KERNEL_CAP_PERSIST is NOT set: the persistence channel is inactive
    // (AV-12 — the frozen ABI §20.1 table lists bit 3 = 0 for v1.0.x).
    (kctl_base as *mut u64).write_volatile(ABI_MAGIC);
    au32(KCTL_ABI_VERSION).store(ABI_VERSION, Ordering::SeqCst);
    au32(KCTL_GENERATION).store(generation, Ordering::SeqCst);
    au32(KCTL_KERNEL_CAPS).store(KERNEL_CAP_FS_V1, Ordering::SeqCst);
    au32(KCTL_MAX_ENDPOINTS).store(MAX_ENDPOINTS, Ordering::SeqCst);
    au32(KCTL_ENDPOINT_STRIDE).store(EP_STRIDE, Ordering::SeqCst);
    au32(KCTL_TRANSPORT_BASE).store(TRANSPORT_BASE, Ordering::SeqCst);
    au32(KCTL_KERNEL_HEAP_BASE).store(KERNEL_HEAP_BASE, Ordering::SeqCst);
    au64(KCTL_HEARTBEAT).store(now_ms, Ordering::SeqCst);
    au32(KCTL_DOORBELL_OFFSET).store(KERNEL_DOORBELL_OFFSET, Ordering::SeqCst);
    au32(KCTL_PAGES_CURRENT).store(mem_pages as u32, Ordering::SeqCst);
    au32(KCTL_PAGES_MAX).store(32_768, Ordering::SeqCst); // 2 GiB (matches supervisor.mjs ksm maximum; VFS ceiling 1536 MiB)

    // Kernel doorbell starts at 0.
    au32(KERNEL_DOORBELL_OFFSET).store(0, Ordering::SeqCst);

    // Endpoint table: all slots FREE; endpoint 0 allocated for pid 1.
    let mut i: u32 = 0;
    while i < MAX_ENDPOINTS {
        let slot = ENDPOINT_TABLE_OFFSET + i * 64;
        au64(slot + EPS_MAGIC).store(ENDPOINT_SLOT_MAGIC, Ordering::SeqCst);
        au32(slot + EPS_EP_BASE).store(0, Ordering::SeqCst);
        au32(slot + EPS_PID).store(0, Ordering::SeqCst);
        au32(slot + EPS_GENERATION).store(0, Ordering::SeqCst);
        au32(slot + EPS_STATE).store(ENDPOINT_STATE_FREE, Ordering::SeqCst);
        au32(slot + EPS_CAPS).store(0, Ordering::SeqCst);
        i += 1;
    }

    // Endpoint 0: block + region + slot (state published last, §5.4).
    // P2-005-004b: the block sits at its FIXED slot 0 (documented layout) —
    // map the whole fixed block region + scratch first so no later lazy
    // endpoint block ever grows the memory.
    if !ensure_mapped(KALLOC_BASE) {
        return uv::ENOMEM; // honest resource error (§5.6)
    }
    let blk0 = ep_block_addr(0);
    zero_bytes(blk0, BK_SIZE);
    write_u64(blk0 + BK_MAGIC_OFF, BK_MAGIC);
    write_u64(blk0 + BK_CANARY_OFF, BK_CANARY);
    write_u32(EPDIR_BASE + EPD_BLOCK_OFF, blk0);
    write_u32(EPDIR_BASE + EPD_FLAGS_OFF, EPD_FLAG_ALLOCATED);
    write_u32(EPDIR_BASE + EPD_GEN_OFF, 1);

    endpoint_region_init(0, 1, 1, ENDPOINT_CAP_SYNC | ENDPOINT_CAP_ASYNC);
    let slot0 = ENDPOINT_TABLE_OFFSET;
    au32(slot0 + EPS_EP_BASE).store(TRANSPORT_BASE, Ordering::SeqCst);
    au32(slot0 + EPS_PID).store(1, Ordering::SeqCst);
    au32(slot0 + EPS_GENERATION).store(1, Ordering::SeqCst);
    au32(slot0 + EPS_CAPS)
        .store(ENDPOINT_CAP_SYNC | ENDPOINT_CAP_ASYNC, Ordering::SeqCst);
    au32(slot0 + EPS_STATE).store(ENDPOINT_STATE_RUNNING, Ordering::SeqCst);

    // -----------------------------------------------------------------------
    // P2-005-004b: kernel heap allocator + VFS boot. The state publish moves
    // BELOW this block — RUNNING must mean "everything is up".
    // -----------------------------------------------------------------------

    // Eager-reserve the kalloc floor (16 MiB — untouched pages cost nothing,
    // WMM §3.4c/M4c) and initialize the allocator over the KSM remainder.
    if !ensure_mapped(KALLOC_BASE.wrapping_add(KALLOC_FLOOR)) {
        return uv::ENOMEM;
    }
    let mem_end = read_u32(KH_MEM_END_OFF);
    match Kalloc::init(KALLOC_BASE as *mut u8, (mem_end - KALLOC_BASE) as usize) {
        Ok(_) => write_u32(KH_KALLOC_OFF, KALLOC_BASE),
        Err(_) => return uv::ENOMEM, // KSM cannot host the floor — honest failure
    }

    // VFS boot: the single tree (spec §1) + the Phase-2 layout
    // (FILESYSTEM.md §2). Mounts are st_dev LABELS only (ruling 0-4 — one
    // tree, never a device boundary): "/project" dev 1, "/root" dev 2 are
    // the frozen spec §1 mount pair (pnpm store ↔ project hardlinks,
    // A-7.5); "/usr" and "/tmp" exist as plain directories (the root
    // mount occupies the third MountEntry; one slot stays spare).
    let now_ns = (now_ms as i64).saturating_mul(1_000_000);
    let vfs_layout = AllocLayout::new::<Vfs>();
    let vfs_ptr = rust_raw_alloc(vfs_layout);
    if vfs_ptr.is_null() {
        return uv::ENOMEM; // unreachable in practice: ~1 KB inside a 16 MiB floor
    }
    // The Vfs construction itself allocates (Vec growth — same floor).
    let vfs: &mut Vfs = unsafe {
        let vp = vfs_ptr as *mut Vfs;
        vp.write(Vfs::new(now_ns));
        &mut *vp
    };
    let boot = vfs
        .ensure_mount(b"/project", 1, now_ns)
        .and_then(|_| vfs.ensure_mount(b"/root", 2, now_ns))
        .and_then(|_| vfs.mkdir(0, b"/usr", 0o755, 0, now_ns))
        .and_then(|_| vfs.mkdir(0, b"/tmp", 0o755, 0, now_ns));
    if boot.is_err() {
        return uv::ENOMEM;
    }
    write_u32(KH_VFS_OFF, vfs_ptr as u32);

    // -----------------------------------------------------------------------
    // P2-005-005 (KH_LAYOUT_VERSION 3): FD subsystem boot — the shared stdio
    // device triple is minted here (ENFILE baseline live = 3), and pid 1's
    // ProcessRecord is born (boot path: endpoint 0 went RUNNING without
    // PROC_READY — FD_SPEC §8). Failure = honest ENOMEM fail-fast (FC-5).
    // -----------------------------------------------------------------------
    let fd_layout = AllocLayout::new::<FdState>();
    let fd_ptr = rust_raw_alloc(fd_layout);
    if fd_ptr.is_null() {
        return uv::ENOMEM;
    }
    {
        let fds: &mut FdState = unsafe {
            let fp = fd_ptr as *mut FdState;
            fp.write(FdState::new());
            &mut *fp
        };
        if fds.process_birth(vfs, 1, kernel_vfs::FD_DEFAULT_UMASK).is_err() {
            return uv::ENOMEM;
        }
    }
    write_u32(KH_FDSTATE_OFF, fd_ptr as u32);

    // Publish the state LAST (§15.4 readers load kernel_state first).
    au32(KCTL_KERNEL_STATE).store(KERNEL_STATE_RUNNING, Ordering::SeqCst);
    0
}

/// Initialize (or re-initialize, on recycle) an endpoint's live region:
/// header, ring headers, mailbox, heartbeat. Caller publishes the endpoint
/// table slot state afterwards.
unsafe fn endpoint_region_init(ep_index: u32, pid: u32, generation: u32, caps: u32) {
    let ep = TRANSPORT_BASE.wrapping_add(ep_index.wrapping_mul(EP_STRIDE));
    au64(ep + EPH_MAGIC).store(ENDPOINT_REGION_MAGIC, Ordering::SeqCst);
    au32(ep + EPH_PID).store(pid, Ordering::SeqCst);
    au32(ep + EPH_GENERATION).store(generation, Ordering::SeqCst);
    au32(ep + EPH_STATE).store(ENDPOINT_STATE_ALLOCATED, Ordering::SeqCst);
    au64(ep + EPH_HEARTBEAT).store(0, Ordering::SeqCst);
    let srh = ep + EP_SUBMIT_RING_HEADER_OFF;
    au32(srh + RH_HEAD).store(0, Ordering::SeqCst);
    au32(srh + RH_TAIL).store(0, Ordering::SeqCst);
    au32(srh + RH_EXTRA).store(0, Ordering::SeqCst);
    let crh = ep + EP_COMPLETION_RING_HEADER_OFF;
    au32(crh + RH_HEAD).store(0, Ordering::SeqCst);
    au32(crh + RH_TAIL).store(0, Ordering::SeqCst);
    au32(crh + RH_EXTRA).store(0, Ordering::SeqCst); // cr_seq = 0
    let smb = ep + EP_SYNC_MAILBOX_OFF;
    au32(smb + SM_SEQ).store(0, Ordering::SeqCst);
    au32(smb + SM_STATE).store(SM_STATE_IDLE, Ordering::SeqCst);
    let _ = caps;
}

// ---------------------------------------------------------------------------
// request validation (ABI §19.2, v1.0.2)
// ---------------------------------------------------------------------------

struct RawRequest {
    req_id: u64,
    op: u32,
    flags: u32,
    tx_off: u32,
    tx_len: u32,
    a0: u64,
    a1: u64,
    a2: u64,
}

/// Validate a dequeued RequestRecord. Returns `Err(NODINE_*)` on violation
/// (explicit failure, never UB, never silent). Unknown ops are
/// `NODINE_ENOOP` (§19.2 item 1 — v1.0.2 correction of the P2-003 behavior
/// which returned NODINE_EINVAL_REQUEST); `tx_off` is TX-arena-relative
/// (§6.1 — v1.0.2 correction); unused args of implemented ops must be zero
/// (§19.2 item 3); sync-mode OP_CANCEL is rejected (§29/ADR-0010 D4).
unsafe fn validate_request(slot: u32, is_sync: bool, ep_base: u32) -> Result<RawRequest, i32> {
    let op = read_u32(slot + REQ_OP);
    let flags = read_u32(slot + REQ_FLAGS);
    let tx_off = read_u32(slot + REQ_TX_OFF);
    let tx_len = read_u32(slot + REQ_TX_LEN);
    let a0 = read_u64(slot + REQ_A0);
    let a1 = read_u64(slot + REQ_A1);
    let a2 = read_u64(slot + REQ_A2);
    let a3 = read_u64(slot + REQ_A3);
    let a4 = read_u64(slot + REQ_A4);

    // §19.2 item 1: op must be a defined id (or a reserved-range id that
    // dispatches to the explicit ENOOP stub).
    let known = matches!(op,
        0x0001..=0x000F      // control
        | 0x0010..=0x001F    // process lifecycle (incl. Phase-3 stubs)
        | 0x00F0..=0x00FF    // meta (Cancel 0x00F0)
        | 0x0100..=0x011F    // filesystem v1 (incl. watch stubs)
        | 0x0120..=0x012F    // fs deferred surface (reserved; ENOOP)
        | 0x0130..=0x013F    // v1.1 fused one-shots (reserved; ENOOP)
        | OP_INGRESS_READ | OP_INGRESS_RESOLVE
        | 0x0200..=0x02FF    // Phase 3 reserved
        | 0x0300..=0x03FF    // Phase 5 vTCP reserved
        | OP_RANGE_DIAG_LO..=OP_RANGE_DIAG_HI // v1.0.2 diagnostic range
    );
    if !known {
        return Err(NODINE_ENOOP); // §19.2 item 1 (v1.0.2 correction)
    }
    // §6.1/§8.4: unknown flag bits rejected.
    if flags & !FLAGS_KNOWN_MASK != 0 {
        return Err(NODINE_EINVAL_REQUEST);
    }
    // ADR-0010 D4: OP_CANCEL rides the async lane only.
    if is_sync && op == Op::Cancel as u32 {
        return Err(NODINE_EINVAL_REQUEST);
    }
    // §19.2 item 3: unused args of implemented ops must be zero. The fs
    // surface gained its implementations in P2-005-006 — per-op arg-zero
    // checks + item-5 range checks live here so invalid requests never
    // allocate (paths/areas are additionally re-checked at dispatch —
    // defense in depth).
    let area_cap = if is_sync { SYNCRES_SIZE - 64 } else { RX_SIZE - 64 };
    let arg_err = match op {
        0x0001 => a1 != 0 || a2 != 0 || a3 != 0 || a4 != 0,          // ABI_PROBE
        0x0002 => a0 != 0 || a1 != 0 || a2 != 0 || a3 != 0 || a4 != 0, // CAPS_QUERY
        0x0010 => a1 != 0 || a2 != 0 || a3 != 0 || a4 != 0,          // PROC_READY (a1 = flags = 0)
        0x0011 => a1 != 0 || a2 != 0 || a3 != 0 || a4 != 0,          // PROC_EXIT (a0 = code)
        0x00F0 => a1 != 0 || a2 != 0 || a3 != 0 || a4 != 0,          // CANCEL (a0 = target)
        OP_TRANSPORT_PROBE => {
            let cap = if is_sync { PROBE_WORK_MAX_SYNC } else { PROBE_WORK_MAX_ASYNC };
            a3 != 0 || a4 != 0 || a2 > cap
        }
        // ---- filesystem v1 (P2-005-006 dispatch batch) ----
        0x0100 => a2 != 0 || a3 != 0 || a4 != 0 || a1 > 0o7777,      // OPEN (a0=flags, a1=mode)
        0x0101 => a1 != 0 || a2 != 0 || a3 != 0 || a4 != 0 || a0 > u32::MAX as u64, // CLOSE
        0x0102 => a3 != 0 || a4 != 0 || a1 > area_cap as u64,        // READ (a1=len_max ≤ area cap)
        0x0103 => a2 != 0 || a3 != 0 || a4 != 0 || tx_len > TX_SIZE - 64, // WRITE
        0x0104 | 0x0105 => a0 != 0 || a1 != 0 || a2 != 0 || a3 != 0 || a4 != 0, // STAT/LSTAT
        0x0106 => a1 != 0 || a2 != 0 || a3 != 0 || a4 != 0,          // FSTAT
        0x0107 => a2 != 0 || a3 != 0 || a4 != 0 || a1 > 0o7777,      // MKDIR (a0=mode)
        0x0108 | 0x0109 | 0x010A => a0 != 0 || a1 != 0 || a2 != 0 || a3 != 0 || a4 != 0, // RMDIR/UNLINK/RENAME
        0x010B => a3 != 0 || a4 != 0
            || a1 > 1024                                              // READDIR max_entries
            || a2 < 512 || a2 > area_cap as u64,                     // READDIR max_bytes (progress floor: ≥ 512 B ⇒ the forced-first-entry always fits)
        0x010C => a2 != 0 || a3 != 0 || a4 != 0,                     // FTRUNCATE (a0=fd, a1=len)
        // ---- P2-006-007 dispatch batch ----
        0x0015 => a0 != 0 || a1 != 0 || a2 != 0 || a3 != 0 || a4 != 0, // PROC_CHDIR (PATH only)
        0x0121 => a1 != 0 || a2 != 0 || a3 != 0 || a4 != 0,          // TRUNCATE (PATH, a0=len)
        0x0122 => a1 != 0 || a2 != 0 || a3 != 0 || a4 != 0 || a0 > 0o7777, // CHMOD (PATH, a0=mode)
        0x0123 => a3 != 0 || a4 != 0 || a2 > 1,                       // UTIMES (PATH, a0=atime_ns, a1=mtime_ns, a2=follow)
        0x0124 => a0 != 0 || a1 != 0 || a2 != 0 || a3 != 0 || a4 != 0, // LINK (PATH2)
        0x0125 => a0 != 0 || a1 != 0 || a2 != 0 || a3 != 0 || a4 != 0, // SYMLINK (PATH2 [target][NUL][linkpath])
        0x0126 => a1 != 0 || a2 != 0 || a3 != 0 || a4 != 0 || a0 > area_cap as u64, // READLINK (PATH, a0=len_max ≤ area cap)
        _ => false, // stubs: not enforced (see doc comment)
    };
    if arg_err {
        return Err(NODINE_EINVAL_REQUEST);
    }
    // Transport-probe result length bound (dispatch would also check; do it
    // here so invalid requests never allocate).
    if op == OP_TRANSPORT_PROBE {
        let len = a0 as u32;
        let max = if is_sync { SYNCRES_SIZE - 64 } else { RX_SIZE - 64 };
        if len > max {
            return Err(NODINE_EINVAL_REQUEST);
        }
    }
    // §19.2 item 4 (mechanism; path content is P2-5 scope): TX bounds +
    // alignment, ARENA-relative (§6.1 — v1.0.2 correction).
    if tx_len > 0 {
        if tx_off & 0x7 != 0 {
            return Err(NODINE_EINVAL_REQUEST);
        }
        if tx_off.saturating_add(tx_len) > TX_SIZE {
            return Err(NODINE_EINVAL_REQUEST);
        }
        // Sync payloads must fit the guaranteed reserve (§11.6 v1.0.1).
        if is_sync && tx_len > TX_SYNC_PAYLOAD_MAX {
            return Err(NODINE_EINVAL_REQUEST);
        }
    }
    // §19.2 item 4 (path content — fs surface live since P2-005-006):
    // PATH/PATH2 payloads are UTF-8, ≤ PATH_MAX per path component pair.
    // ENAMETOOLONG is a DOMAIN error (-36), invalid UTF-8 is EINVAL (-22);
    // both flow through the normal error-completion path. Runs AFTER the TX
    // bounds check above — the slice is only taken over validated bytes.
    let takes_path = matches!(op,
        0x0100 | 0x0104 | 0x0105 | 0x0107 | 0x0108 | 0x0109 | 0x010B // PATH
        | 0x0015 | 0x0121 | 0x0122 | 0x0123 | 0x0126);               // P2-006-007 PATH set
    let is_rename = op == 0x010A;
    // PATH2 (two NUL-separated paths): RENAME [old][NUL][new], LINK
    // [old][NUL][new], SYMLINK [target][NUL][linkpath] (P2-006-007).
    let is_path2 = is_rename || op == 0x0124 || op == 0x0125;
    if takes_path || is_path2 {
        if tx_len == 0 {
            return Err(uv::EINVAL); // empty path payload
        }
        let bytes = core::slice::from_raw_parts(
            (ep_base + EP_TX_ARENA_OFF + tx_off) as *const u8,
            tx_len as usize,
        );
        if is_path2 {
            // PATH2: [old][NUL][new] — POSIX paths cannot contain NUL, so the
            // split on the FIRST NUL is unambiguous (§6.1).
            let Some(nul) = bytes.iter().position(|&b| b == 0) else {
                return Err(uv::EINVAL);
            };
            let (old_p, rest) = bytes.split_at(nul);
            let new_p = &rest[1..];
            if old_p.is_empty() || new_p.is_empty() {
                return Err(uv::EINVAL);
            }
            if core::str::from_utf8(old_p).is_err() || core::str::from_utf8(new_p).is_err() {
                return Err(uv::EINVAL);
            }
            if old_p.len() > PATH_MAX as usize || new_p.len() > PATH_MAX as usize {
                return Err(uv::ENAMETOOLONG);
            }
        } else {
            if tx_len > PATH_MAX {
                return Err(uv::ENAMETOOLONG);
            }
            if core::str::from_utf8(bytes).is_err() {
                return Err(uv::EINVAL);
            }
        }
    }
    Ok(RawRequest { req_id: read_u64(slot + REQ_REQ_ID), op, flags, tx_off, tx_len, a0, a1, a2 })
}

// ---------------------------------------------------------------------------
// dispatch (the VFS arrives P2-005; every other op returns NODINE_ENOOP)
// ---------------------------------------------------------------------------

struct Outcome {
    status: i32,
    kind: u32, // RESULT_*
    inline_: [u8; 16],
    len: u32,
    /// RX arena span consumed by this result (async payloads; 0 otherwise).
    rx_span: u32,
    /// RX arena offset of this result's span (rx_span > 0 only).
    rx_off: u32,
    /// sync-result-area length (sync payloads; 0 otherwise)
    sr_len: u32,
    /// true if the op was aborted by a cooperative cancel (§18/§29)
    aborted: bool,
}

impl Outcome {
    fn none(status: i32) -> Outcome {
        Outcome { status, kind: RESULT_NONE, inline_: [0; 16], len: 0, rx_span: 0, rx_off: 0, sr_len: 0, aborted: false }
    }
}

/// RX-arena allocator: a bump byte-ring with FULL-DRAIN RESET — §6.2/§19.2
/// require every record LINEAR-contiguous (result_off + result_len ≤ RX_SIZE),
/// so a span never wraps past the arena end and no restart-at-0 gaps exist
/// (gap arithmetic is unrecoverable with a byte ledger: live spans may sit
/// linearly above rx_tail). Discipline:
///   - when every span is retired (unretired == 0) the arena is wholly free:
///     the next allocation resets the tail to 0;
///   - otherwise a span allocates at rx_tail iff it fits linearly;
///   - anything else defers (§17.2) until retire-driven progress — bounded
///     by the ASYNC_DEFER_LIMIT EAGAIN conversion, so liveness holds.
/// Tail capacity below `need` at the end of an arena cycle is deferred
/// latency (never deadlock) — the documented cost of linear records.
unsafe fn rx_alloc_probe(blk: u32, need: u32) -> Option<u32> {
    let rx_tail = read_u32(blk + BK_RX_TAIL_OFF);
    let unretired = read_u32(blk + BK_UNRETIRED_OFF);
    if unretired == 0 {
        return Some(0); // fully drained: reset the bump tail
    }
    if rx_tail.wrapping_add(need) <= RX_SIZE {
        Some(rx_tail)
    } else {
        None
    }
}

/// Commit the allocation (advances rx_tail). 0xFFFF_FFFF = cannot fit
/// (caller gate-checked; unreachable on the serialized kernel).
unsafe fn rx_alloc(blk: u32, need: u32) -> u32 {
    match rx_alloc_probe(blk, need) {
        Some(off) => {
            write_u32(blk + BK_RX_TAIL_OFF, off.wrapping_add(need));
            off
        }
        None => 0xFFFF_FFFF,
    }
}

/// Execute the diagnostic transport probe (ADR-0010 D2). Work units run
/// BEFORE the result is materialized so a cooperative cancel aborts without
/// touching the ledger (P2_004_CANCELLATION.md race matrix).
///
/// # Safety
/// `ep_base` + arenas must be inside the linear memory; `blk` is the
/// endpoint's kernel-heap block; `srh` the submit-ring header (for the
/// cooperative re-scan); `tail_at_start` bounds the re-scan window.
unsafe fn dispatch_transport_probe(
    req: &RawRequest,
    is_sync: bool,
    ep_base: u32,
    blk: u32,
    srh: u32,
    tail_at_start: u32,
) -> Outcome {
    let len = req.a0 as u32;
    let fill = (req.a1 & 0xFF) as u8;
    let work = req.a2;

    // Cooperative work loop: each unit is a 64-byte scratch fill + a cancel
    // re-scan of submit-ring entries published since this pass began
    // (§18 "may check the cancel table between op phases"; ADR-0010 D4).
    if work > 0 {
        write_u64(blk + BK_EXEC_ID_OFF, req.req_id);
        let mut unit: u64 = 0;
        while unit < work {
            fill_bytes(KH_SCRATCH_OFF + ((unit as u32) & 0x3F) * 64, 64, fill);
            let tail_now = au32(srh + RH_TAIL).load(Ordering::SeqCst);
            let mut idx = tail_at_start;
            while idx != tail_now {
                let s = ep_ring_slot(ep_base, EP_SUBMIT_SLOTS_OFF, idx);
                if read_u32(s + REQ_OP) == Op::Cancel as u32
                    && read_u64(s + REQ_A0) == req.req_id
                    && read_u32(s + REQ_FLAGS) & FLAG_MODE_SYNC == 0
                {
                    write_u64(blk + BK_EXEC_ID_OFF, 0);
                    return Outcome::none(uv::ECANCELED); // aborted, nothing allocated
                }
                idx = idx.wrapping_add(1);
            }
            unit += 1;
        }
        write_u64(blk + BK_EXEC_ID_OFF, 0);
    }

    if len <= INLINE_CAP {
        let mut inline_ = [0u8; 16];
        let mut i = 0usize;
        while i < len as usize {
            inline_[i] = fill;
            i += 1;
        }
        return Outcome { status: 0, kind: RESULT_INLINE, inline_, len, rx_span: 0, rx_off: 0, sr_len: 0, aborted: false };
    }
    if is_sync {
        // §9.4: single-occupancy, always empty — write at offset 0.
        fill_bytes(ep_base + EP_SYNCRES_OFF, len, fill);
        Outcome { status: 0, kind: RESULT_SYNCRES, inline_: [0; 16], len, rx_span: 0, rx_off: 0, sr_len: len, aborted: false }
    } else {
        // RX allocation. §6.2/§19.2 make a record's bytes LINEAR-contiguous
        // (result_off + result_len ≤ RX_SIZE): a span never wraps past the
        // arena end. When the tail cannot fit the span, the allocation
        // restarts at 0 and the residual tail gap stays free (reclaimed by
        // the ledger once the spans before it retire). Space was gate-checked
        // at dequeue and RX free space is monotonic under the serialized
        // kernel (P2_004_BACKPRESSURE.md §5), so rx_alloc_probe's decision
        // still holds here.
        let need = (len + 7) & !7; // 8-aligned span
        let off = rx_alloc(blk, need);
        if off == 0xFFFF_FFFF {
            // Structurally unreachable (lane 2 gates with the same probe and
            // RX free only grows under the serialized kernel) — fail
            // explicitly rather than corrupt memory (§19.1).
            return Outcome::none(uv::ENOMEM);
        }
        let arena = ep_base + EP_RX_ARENA_OFF;
        fill_bytes(arena + off, len, fill);
        Outcome {
            status: 0,
            kind: RESULT_RX_ARENA,
            inline_: [0; 16],
            len,
            rx_span: need,
            sr_len: 0,
            aborted: false,
            rx_off: off,
        }
    }
}

// ---------------------------------------------------------------------------
// filesystem dispatch (P2-005-006 — the ABI §7.3 v1 op set against the REAL
// FdState + Vfs; FD semantics live in kernel-vfs, NEVER re-implemented here)
// ---------------------------------------------------------------------------

/// Stat encoding (§7.3): 18 slots × 8 B at the result-area base. Default
/// f64 (Node's non-bigint Stats); `FLAG_STAT_BIGINT` switches to i64. Field
/// order is Node's `getStatsFromBinding` — StatData is field-compatible.
fn stat_words(sd: &kernel_vfs::vfs::StatData, bigint: bool) -> [u64; 18] {
    let cf = |v: u64| if bigint { v } else { (v as f64).to_bits() };
    let ci = |v: i64| if bigint { v as u64 } else { (v as f64).to_bits() };
    [
        cf(sd.dev), cf(sd.mode), cf(sd.nlink), cf(sd.uid), cf(sd.gid),
        cf(sd.rdev), cf(sd.blksize), cf(sd.ino), cf(sd.size), cf(sd.blocks),
        ci(sd.atime_sec), ci(sd.atime_nsec as i64),
        ci(sd.mtime_sec), ci(sd.mtime_nsec as i64),
        ci(sd.ctime_sec), ci(sd.ctime_nsec as i64),
        ci(sd.birthtime_sec), ci(sd.birthtime_nsec as i64),
    ]
}

/// Write a 144 B stat result into the sync result area (offset 0, §9.4).
unsafe fn write_stat_syncres(ep_base: u32, sd: &kernel_vfs::vfs::StatData, bigint: bool) {
    let words = stat_words(sd, bigint);
    let base = ep_base + EP_SYNCRES_OFF;
    let mut i = 0;
    while i < 18 {
        write_u64(base + (i as u32) * 8, words[i as usize]);
        i += 1;
    }
}

/// READDIR wire batch (P2-005-006 pin — the ABI §7.3 format leaves the
/// header padding open; this is the canonical layout, mirrored by the
/// bridge decoder):
///   +0  u32 count
///   +4  u32 pad (reserved, 0)
///   +8  u64 next_cookie (echo of the request cookie at EOF / count = 0)
///   +16 entries, each 8-aligned:
///     u8 kind (UV_DIRENT) · u8 pad[3] · u32 name_len · u8 name[..] padded to 8
const READDIR_HEADER: u32 = 16;
const READDIR_ENTRY_FIXED: u32 = 8;

/// Execute one filesystem op. `blk` is the endpoint's kernel-heap block;
/// `ep_base` the endpoint region base. The request has passed
/// `validate_request` (arg zeros, path UTF-8/length, area caps, TX bounds).
///
/// # Safety
/// `ep_base`/`blk` must belong to a live endpoint; the TX slice over
/// `tx_off..tx_off+tx_len` was bounds-validated by the caller.
unsafe fn dispatch_fs(req: &RawRequest, ep_index: u32, ep_base: u32, blk: u32, is_sync: bool) -> Outcome {
    let slot = ENDPOINT_TABLE_OFFSET + ep_index * 64;
    let pid = au32(slot + EPS_PID).load(Ordering::SeqCst);
    // now_ns: the pump stores its caller's clock at every pass start; the
    // driver refreshes it ≤250 ms — mtime/ctime granularity is the pump's.
    let now_ns = (au64(KCTL_HEARTBEAT).load(Ordering::SeqCst) as i64).saturating_mul(1_000_000);
    let (fdst, vfs) = match (fdstate_mut(), vfs_mut()) {
        (Some(f), Some(v)) => (f, v),
        _ => return Outcome::none(uv::ENOMEM), // not booted — cannot happen post-init
    };
    let tx: &[u8] = if req.tx_len > 0 {
        core::slice::from_raw_parts((ep_base + EP_TX_ARENA_OFF + req.tx_off) as *const u8, req.tx_len as usize)
    } else {
        &[]
    };

    match req.op {
        // ---- OP_OPEN: PATH; a0=flags, a1=mode ⇒ INLINE u32 fd ----------
        0x0100 => match fdst.open(vfs, pid, tx, req.a0 as u32, req.a1 as u16, now_ns) {
            Ok(fd) => {
                let mut inline_ = [0u8; 16];
                inline_[0..4].copy_from_slice(&fd.to_le_bytes());
                Outcome { status: 0, kind: RESULT_INLINE, inline_, len: 4, rx_span: 0, rx_off: 0, sr_len: 0, aborted: false }
            }
            Err(e) => Outcome::none(e.code()),
        },

        // ---- OP_CLOSE: a0=fd ⇒ NONE ------------------------------------
        0x0101 => match fdst.close(vfs, pid, req.a0 as u32) {
            Ok(()) => Outcome::none(0),
            Err(e) => Outcome::none(e.code()),
        },

        // ---- OP_READ: a0=fd, a1=len_max, a2=pos ⇒ AREA bytes ------------
        0x0102 => {
            let (fd, len_max, pos) = (req.a0 as u32, req.a1 as u32, req.a2 as i64);
            if is_sync {
                let cap = SYNCRES_SIZE - 64;
                if len_max > cap {
                    return Outcome::none(uv::EINVAL); // re-checked (defense in depth)
                }
                let buf = core::slice::from_raw_parts_mut((ep_base + EP_SYNCRES_OFF) as *mut u8, len_max as usize);
                match fdst.read(vfs, pid, fd, pos, buf, now_ns) {
                    Ok(0) => Outcome::none(0), // EOF: kind NONE, len 0 (§7.3)
                    Ok(n) => Outcome { status: 0, kind: RESULT_SYNCRES, inline_: [0; 16], len: n as u32, rx_span: 0, rx_off: 0, sr_len: n as u32, aborted: false },
                    Err(e) => Outcome::none(e.code()),
                }
            } else {
                if len_max == 0 {
                    return Outcome::none(0); // zero-length read: immediate EOF shape
                }
                let need = (len_max + 7) & !7;
                let off = rx_alloc(blk, need);
                if off == 0xFFFF_FFFF {
                    return Outcome::none(uv::ENOMEM); // unreachable (lane-2 gate)
                }
                let buf = core::slice::from_raw_parts_mut((ep_base + EP_RX_ARENA_OFF + off) as *mut u8, len_max as usize);
                match fdst.read(vfs, pid, fd, pos, buf, now_ns) {
                    Ok(0) => Outcome { status: 0, kind: RESULT_NONE, inline_: [0; 16], len: 0, rx_span: need, rx_off: off, sr_len: 0, aborted: false },
                    Ok(n) => Outcome { status: 0, kind: RESULT_RX_ARENA, inline_: [0; 16], len: n as u32, rx_span: need, rx_off: off, sr_len: 0, aborted: false },
                    Err(e) => Outcome { status: e.code(), kind: RESULT_NONE, inline_: [0; 16], len: 0, rx_span: need, rx_off: off, sr_len: 0, aborted: false },
                }
            }
        }

        // ---- OP_WRITE: a0=fd, a1=pos, TX=data ⇒ INLINE u32 n ------------
        0x0103 => match fdst.write(vfs, pid, req.a0 as u32, req.a1 as i64, tx, now_ns) {
            Ok(n) => {
                let mut inline_ = [0u8; 16];
                inline_[0..4].copy_from_slice(&(n as u32).to_le_bytes());
                Outcome { status: 0, kind: RESULT_INLINE, inline_, len: 4, rx_span: 0, rx_off: 0, sr_len: 0, aborted: false }
            }
            Err(e) => Outcome::none(e.code()),
        },

        // ---- OP_STAT / OP_LSTAT: PATH ⇒ AREA 144 B ----------------------
        0x0104 | 0x0105 => {
            let bigint = req.flags & FLAG_STAT_BIGINT != 0;
            let cwd = match fdst.cwd_umask_of(pid) {
                Some(x) => x.0,
                None => return Outcome::none(uv::EBADF),
            };
            let res = if req.op == 0x0104 { vfs.stat(cwd, tx) } else { vfs.lstat(cwd, tx) };
            match res {
                Ok(sd) => {
                    if is_sync {
                        write_stat_syncres(ep_base, &sd, bigint);
                        Outcome { status: 0, kind: RESULT_SYNCRES, inline_: [0; 16], len: 144, rx_span: 0, rx_off: 0, sr_len: 144, aborted: false }
                    } else {
                        let off = rx_alloc(blk, 144);
                        if off == 0xFFFF_FFFF {
                            return Outcome::none(uv::ENOMEM);
                        }
                        let words = stat_words(&sd, bigint);
                        let base = ep_base + EP_RX_ARENA_OFF + off;
                        let mut i = 0;
                        while i < 18 {
                            write_u64(base + (i as u32) * 8, words[i as usize]);
                            i += 1;
                        }
                        Outcome { status: 0, kind: RESULT_RX_ARENA, inline_: [0; 16], len: 144, rx_span: 144, rx_off: off, sr_len: 0, aborted: false }
                    }
                }
                Err(e) => Outcome::none(e.code()),
            }
        }

        // ---- OP_FSTAT: a0=fd ⇒ AREA 144 B -------------------------------
        0x0106 => {
            let bigint = req.flags & FLAG_STAT_BIGINT != 0;
            match fdst.fstat(vfs, pid, req.a0 as u32) {
                Ok(sd) => {
                    if is_sync {
                        write_stat_syncres(ep_base, &sd, bigint);
                        Outcome { status: 0, kind: RESULT_SYNCRES, inline_: [0; 16], len: 144, rx_span: 0, rx_off: 0, sr_len: 144, aborted: false }
                    } else {
                        let off = rx_alloc(blk, 144);
                        if off == 0xFFFF_FFFF {
                            return Outcome::none(uv::ENOMEM);
                        }
                        let words = stat_words(&sd, bigint);
                        let base = ep_base + EP_RX_ARENA_OFF + off;
                        let mut i = 0;
                        while i < 18 {
                            write_u64(base + (i as u32) * 8, words[i as usize]);
                            i += 1;
                        }
                        Outcome { status: 0, kind: RESULT_RX_ARENA, inline_: [0; 16], len: 144, rx_span: 144, rx_off: off, sr_len: 0, aborted: false }
                    }
                }
                Err(e) => Outcome::none(e.code()),
            }
        }

        // ---- OP_MKDIR: PATH; a0=mode ⇒ NONE -----------------------------
        0x0107 => {
            let (cwd, umask) = match fdst.cwd_umask_of(pid) {
                Some(x) => x,
                None => return Outcome::none(uv::EBADF),
            };
            match vfs.mkdir(cwd, tx, req.a0 as u16, umask, now_ns) {
                Ok(_) => Outcome::none(0),
                Err(e) => Outcome::none(e.code()),
            }
        }

        // ---- OP_RMDIR / OP_UNLINK: PATH ⇒ NONE --------------------------
        0x0108 | 0x0109 => {
            let cwd = match fdst.cwd_umask_of(pid) {
                Some(x) => x.0,
                None => return Outcome::none(uv::EBADF),
            };
            let res = if req.op == 0x0108 { vfs.rmdir(cwd, tx, now_ns) } else { vfs.unlink(cwd, tx, now_ns) };
            match res {
                Ok(()) => Outcome::none(0),
                Err(e) => Outcome::none(e.code()),
            }
        }

        // ---- OP_RENAME: PATH2 [old][NUL][new] ⇒ NONE --------------------
        0x010A => {
            let cwd = match fdst.cwd_umask_of(pid) {
                Some(x) => x.0,
                None => return Outcome::none(uv::EBADF),
            };
            let nul = match tx.iter().position(|&b| b == 0) {
                Some(n) => n,
                None => return Outcome::none(uv::EINVAL),
            };
            let (old_p, rest) = tx.split_at(nul);
            let new_p = &rest[1..];
            match vfs.rename(cwd, old_p, new_p, now_ns) {
                Ok(()) => Outcome::none(0),
                Err(e) => Outcome::none(e.code()),
            }
        }

        // ---- OP_READDIR: PATH; a0=cookie, a1=max_entries, a2=max_bytes ---
        0x010B => {
            let cwd = match fdst.cwd_umask_of(pid) {
                Some(x) => x.0,
                None => return Outcome::none(uv::EBADF),
            };
            let (cookie, max_entries, max_bytes) = (req.a0, req.a1 as usize, req.a2 as u32);
            // Name-byte budget for the VFS walk: reserve the wire overhead
            // (header + 8 B per entry) so the emitted batch always fits the
            // caller's max_bytes; the VFS's forced-first-entry (progress
            // guarantee) fits because validation floors max_bytes at 512.
            let name_budget = (max_bytes.saturating_sub(READDIR_HEADER + READDIR_ENTRY_FIXED * max_entries as u32)) as usize;
            // Pass 1 (measure) — serialized kernel: identical state on both
            // passes, so the second pass writes exactly what the first saw.
            let mut wire = READDIR_HEADER as usize;
            // Pass 1 (measure + validate): errors complete here, BEFORE any
            // area allocation (ENOENT/ENOTDIR never charge a span).
            let measured = match vfs.readdir_with(cwd, tx, cookie, max_entries, name_budget, |name, _kind, _ino, _seq| {
                wire += READDIR_ENTRY_FIXED as usize + ((name.len() + 7) & !7);
            }) {
                Ok(m) => m,
                Err(e) => return Outcome::none(e.code()),
            };
            let _ = measured;
            if is_sync {
                let base = ep_base + EP_SYNCRES_OFF;
                write_readdir_batch(base, vfs, cwd, tx, cookie, max_entries, name_budget);
                Outcome { status: 0, kind: RESULT_SYNCRES, inline_: [0; 16], len: wire as u32, rx_span: 0, rx_off: 0, sr_len: wire as u32, aborted: false }
            } else {
                let need = (wire as u32 + 7) & !7;
                let off = rx_alloc(blk, need);
                if off == 0xFFFF_FFFF {
                    // P2-008 Step-3 reliability fix companion: the §17.2 gate
                    // probes at the 512 B floor, so a measured batch LARGER
                    // than the currently-contiguous free span can reach here
                    // (arena fragmented, not empty). This is transient
                    // capacity pressure, NOT an invalid op: emit EAGAIN
                    // explicitly (uv retry semantics) — never a short batch,
                    // never silent corruption. Structurally rare: the gate's
                    // floor-pass means the arena had ≥ floor contiguous free
                    // at dequeue, and RX free only grows under the serialized
                    // kernel.
                    return Outcome::none(uv::EAGAIN);
                }
                let base = ep_base + EP_RX_ARENA_OFF + off;
                write_readdir_batch(base, vfs, cwd, tx, cookie, max_entries, name_budget);
                Outcome { status: 0, kind: RESULT_RX_ARENA, inline_: [0; 16], len: wire as u32, rx_span: need, rx_off: off, sr_len: 0, aborted: false }
            }
        }

        // ---- OP_FTRUNCATE: a0=fd, a1=len ⇒ NONE (§7.3 "P2-005 optional") -
        0x010C => match fdst.ftruncate(vfs, pid, req.a0 as u32, req.a1, now_ns) {
            Ok(()) => Outcome::none(0),
            Err(e) => Outcome::none(e.code()),
        },

        // ---- OP_PROC_CHDIR (P2-006-007; 0x0015, process range — previously
        // unallocated): PATH ⇒ NONE. The kernel stays the single cwd
        // authority (FD_SPEC process records); the Node process.chdir
        // binding maps here.
        0x0015 => match fdst.chdir(vfs, pid, tx) {
            Ok(()) => Outcome::none(0),
            Err(e) => Outcome::none(e.code()),
        },

        // ---- OP_TRUNCATE (0x0121, fs deferred surface): PATH; a0=len ------
        0x0121 => {
            let cwd = match fdst.cwd_umask_of(pid) {
                Some(x) => x.0,
                None => return Outcome::none(uv::EBADF),
            };
            match vfs.truncate_path(cwd, tx, req.a0, now_ns) {
                Ok(()) => Outcome::none(0),
                Err(e) => Outcome::none(e.code()),
            }
        }

        // ---- OP_CHMOD (0x0122): PATH; a0=mode -----------------------------
        0x0122 => {
            let cwd = match fdst.cwd_umask_of(pid) {
                Some(x) => x.0,
                None => return Outcome::none(uv::EBADF),
            };
            match vfs.chmod(cwd, tx, req.a0 as u16, now_ns) {
                Ok(()) => Outcome::none(0),
                Err(e) => Outcome::none(e.code()),
            }
        }

        // ---- OP_UTIMES (0x0123): PATH; a0=atime_ns, a1=mtime_ns, a2=follow
        // (1 = utimes, 0 = lutimes). Out-of-i64-range inputs were rejected at
        // validate_request (EINVAL) — the bit pattern round-trips here.
        0x0123 => {
            let cwd = match fdst.cwd_umask_of(pid) {
                Some(x) => x.0,
                None => return Outcome::none(uv::EBADF),
            };
            match vfs.utimes(cwd, tx, req.a2 == 1, req.a0 as i64, req.a1 as i64, now_ns) {
                Ok(()) => Outcome::none(0),
                Err(e) => Outcome::none(e.code()),
            }
        }

        // ---- OP_LINK (0x0124): PATH2 [old][NUL][new] ⇒ NONE --------------
        0x0124 => {
            let cwd = match fdst.cwd_umask_of(pid) {
                Some(x) => x.0,
                None => return Outcome::none(uv::EBADF),
            };
            let nul = match tx.iter().position(|&b| b == 0) {
                Some(n) => n,
                None => return Outcome::none(uv::EINVAL),
            };
            let (old_p, rest) = tx.split_at(nul);
            let new_p = &rest[1..];
            match vfs.link(cwd, old_p, new_p, now_ns) {
                Ok(()) => Outcome::none(0),
                Err(e) => Outcome::none(e.code()),
            }
        }

        // ---- OP_SYMLINK (0x0125): PATH2 [target][NUL][linkpath] ⇒ NONE ---
        // POSIX arg order: symlink(target, linkpath) — the TARGET comes first
        // on the wire (matches the Node fs.symlink(target, path) shape).
        0x0125 => {
            let cwd = match fdst.cwd_umask_of(pid) {
                Some(x) => x.0,
                None => return Outcome::none(uv::EBADF),
            };
            let nul = match tx.iter().position(|&b| b == 0) {
                Some(n) => n,
                None => return Outcome::none(uv::EINVAL),
            };
            let (target, rest) = tx.split_at(nul);
            let linkpath = &rest[1..];
            match vfs.symlink(cwd, linkpath, target, now_ns) {
                Ok(_) => Outcome::none(0), // inode idx not carried (ABI: NONE result)
                Err(e) => Outcome::none(e.code()),
            }
        }

        // ---- OP_READLINK (0x0126): PATH; a0=len_max ⇒ AREA target bytes ---
        // Two-pass over the read-only resolver: drain into a stack buffer,
        // then place the result (sync: SYNCRES; async: rx_alloc).
        0x0126 => {
            let len_max = req.a0 as u32;
            let cwd = match fdst.cwd_umask_of(pid) {
                Some(x) => x.0,
                None => return Outcome::none(uv::EBADF),
            };
            let mut tbuf = [0u8; 4096]; // PATH_MAX
            let mut tlen: usize = 0;
            let rl = vfs.readlink_with(cwd, tx, |target: &[u8]| {
                tlen = target.len().min(4096);
                tbuf[..tlen].copy_from_slice(&target[..tlen]);
            });
            match rl {
                Ok(()) => {
                    let n = (tlen as u32).min(len_max);
                    if is_sync {
                        let base = ep_base + EP_SYNCRES_OFF;
                        for i in 0..n {
                            write_u8(base + i, tbuf[i as usize]);
                        }
                        Outcome { status: 0, kind: RESULT_SYNCRES, inline_: [0; 16], len: n, rx_span: 0, rx_off: 0, sr_len: n, aborted: false }
                    } else {
                        let need = (n + 7) & !7;
                        let off = rx_alloc(blk, need);
                        if off == 0xFFFF_FFFF {
                            return Outcome::none(uv::ENOMEM);
                        }
                        let base = ep_base + EP_RX_ARENA_OFF + off;
                        for i in 0..n {
                            write_u8(base + i, tbuf[i as usize]);
                        }
                        Outcome { status: 0, kind: RESULT_RX_ARENA, inline_: [0; 16], len: n, rx_span: need, rx_off: off, sr_len: 0, aborted: false }
                    }
                }
                Err(e) => Outcome::none(e.code()),
            }
        }

        _ => Outcome::none(NODINE_ENOOP), // unreachable (call-site gated)
    }
}

/// Write the measured readdir batch (pass 2 — see dispatch_fs). Pass 1
/// measured `wire` (for rx_alloc); this pass re-walks with IDENTICAL
/// parameters — the serialized kernel guarantees the same emission — and
/// patches the header (count + next_cookie) from its own result.
unsafe fn write_readdir_batch(
    base: u32,
    vfs: &mut Vfs,
    cwd: u32,
    path: &[u8],
    cookie: u64,
    max_entries: usize,
    name_budget: usize,
) {
    write_u32(base, 0); // count — patched below
    write_u32(base + 4, 0); // reserved
    let mut cursor = READDIR_HEADER;
    let res = vfs.readdir_with(cwd, path, cookie, max_entries, name_budget, |name, kind, _ino, _seq| {
        let at = base + cursor;
        write_u32(at, kind as u32); // u8 kind + pad[3] (zero)
        write_u32(at + 4, name.len() as u32);
        let dst = core::slice::from_raw_parts_mut((at + READDIR_ENTRY_FIXED) as *mut u8, name.len());
        dst.copy_from_slice(name);
        cursor += READDIR_ENTRY_FIXED + ((name.len() + 7) & !7) as u32;
    });
    // Header patch: count + next_cookie (at EOF / count = 0 the cookie
    // echoes the request's — count 0 is the EOF signal per §7.3). Pass 2
    // re-walks identical state, so Err is structurally unreachable; an
    // impossible Err still writes an honest empty batch and propagates.
    let (next_cookie, emitted) = match res {
        Ok(x) => x,
        Err(_) => (cookie, 0),
    };
    write_u32(base, emitted as u32);
    write_u64(base + 8, next_cookie);
}

/// Validate-only outcome for ops the kernel does not implement (stubs).
unsafe fn dispatch(req: &RawRequest) -> Outcome {
    if req.op == Op::AbiProbe as u32 {
        // §7.1: a0 = packed bridge abi_version.
        let bridge = req.a0 as u32;
        if abi_compatible(ABI_VERSION, bridge) {
            let mut inline_ = [0u8; 16];
            inline_[0..4].copy_from_slice(&ABI_VERSION.to_le_bytes());
            inline_[4..8].copy_from_slice(&KERNEL_CAP_FS_V1.to_le_bytes());
            return Outcome { status: 0, kind: RESULT_INLINE, inline_, len: 16, rx_span: 0, rx_off: 0, sr_len: 0, aborted: false };
        } else {
            return Outcome::none(NODINE_EABI_VERSION);
        }
    }
    if req.op == Op::CapsQuery as u32 {
        // §7.1 (v1.0.2 correction of the P2-003 ENOOP gap).
        let mut inline_ = [0u8; 16];
        inline_[0..4].copy_from_slice(&KERNEL_CAP_FS_V1.to_le_bytes());
        return Outcome { status: 0, kind: RESULT_INLINE, inline_, len: 4, rx_span: 0, rx_off: 0, sr_len: 0, aborted: false };
    }
    if req.op == OP_TRANSPORT_PROBE {
        // handled by the caller (needs endpoint context); unreachable here
        return Outcome::none(NODINE_EPROTOCOL);
    }
    // Every other op — including the whole fs surface — returns the explicit
    // NODINE_ENOOP stub status (§7.3/§7.5). No silent success.
    Outcome::none(NODINE_ENOOP)
}

// ---------------------------------------------------------------------------
// completion (sync mailbox §11 / completion ring §12 — one schema)
// ---------------------------------------------------------------------------

/// Complete a MODE_SYNC request into the mailbox, §11.2 kernel steps 4–9 with
/// the v1.0.1 complete-CAS residual-state handling (§11.5). Returns true if
/// the endpoint's `sm_seq` must be notified (driver performs the notify).
unsafe fn complete_sync(ep_base: u32, req: &RawRequest, out: &Outcome) -> bool {
    let smb = ep_base + EP_SYNC_MAILBOX_OFF;

    // Steps 4–5: write result fields FIRST — the SeqCst CAS below publishes
    // them (fields-BEFORE-CAS, v1.0.1/F-B1).
    write_u64(smb + SM_REQ_ID, req.req_id);
    write_u32(smb + SM_OP, req.op);
    write_u32(smb + SM_STATUS, out.status as u32);
    write_u32(smb + SM_KIND, out.kind);
    write_u32(smb + SM_OFF, 0);
    write_u32(smb + SM_LEN, out.len);
    write_bytes(smb + SM_INLINE, &out.inline_);

    // Step 6: CAS(SUBMITTED → COMPLETED); failure handling covers every
    // residual state (v1.0.1 F-B2).
    match au32(smb + SM_STATE).compare_exchange(
        SM_STATE_SUBMITTED,
        SM_STATE_COMPLETED,
        Ordering::SeqCst,
        Ordering::SeqCst,
    ) {
        Ok(_) => {}
        Err(actual) => match actual {
            SM_STATE_ABANDONED => {
                let _ = au32(smb + SM_STATE).compare_exchange(
                    SM_STATE_ABANDONED,
                    SM_STATE_IDLE,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                );
                au32(smb + SM_SEQ).fetch_add(1, Ordering::SeqCst);
                return true; // wake the abandoning worker so it can finish
            }
            SM_STATE_COMPLETED => {
                return false; // structurally impossible (serialized kernel)
            }
            _ => {
                return false; // IDLE mismatch: bridge bug; drop + advance
            }
        },
    }

    // Step 7: sm_seq++ (SeqCst) — the sync doorbell; driver notifies.
    au32(smb + SM_SEQ).fetch_add(1, Ordering::SeqCst);
    true
}

/// Consumer-side ledger fault (§19.1/§19.4): flag + reset the ledger to a
/// safe state; never UB, never silent.
unsafe fn ledger_fault_reset(blk: u32, cr_head_obs: u32) {
    let f = read_u32(KH_FLAGS_OFF);
    write_u32(KH_FLAGS_OFF, f | KH_FLAG_LEDGER_FAULT);
    write_u32(blk + BK_UNRETIRED_OFF, 0);
    write_u32(blk + BK_LEDGER_HEAD_OFF, cr_head_obs);
    write_u32(blk + BK_RX_TAIL_OFF, 0);
    zero_bytes(blk + BK_LENS_OFF, RING_SLOTS * 4);
}

/// Retire ledger pops up to the consumer's observed `cr_head` (§9.3: free in
/// retire order; alloc order = retire order ⇒ contiguous un-retired span).
/// A consumer `cr_head` rewind (bridge bug / hostile) can never underflow
/// into UB: the fault is flagged and the ledger reset (§19.1 discipline).
unsafe fn ledger_pop(blk: u32, cr_head_obs: u32) {
    let mut head = read_u32(blk + BK_LEDGER_HEAD_OFF);
    let mut unretired = read_u32(blk + BK_UNRETIRED_OFF);
    if cr_head_obs == head {
        return;
    }
    // P2-004-015 (audit A-7): a legitimate pop distance is bounded by the
    // ring capacity — the kernel only produces completions for this endpoint
    // while pumping it, so between two processing passes the consumer can
    // retire at most RING_SLOTS un-popped entries. A wrapping distance
    // beyond that is a rewind / fabricated head: fault + reset IMMEDIATELY,
    // never walk it — with an all-inline lens history the walk below has no
    // early exit and measured 25.3 s for a 5-entry rewind (a kernel stall
    // long enough to trip the bridge's §21.3 kernel-death ladder;
    // evidence: poc/p2-004/results/t64-probe.txt).
    if cr_head_obs.wrapping_sub(head) > RING_SLOTS {
        ledger_fault_reset(blk, cr_head_obs);
        return;
    }
    while head != cr_head_obs {
        let span = read_u32(blk + BK_LENS_OFF + (head & RING_INDEX_MASK) * 4);
        if span > unretired {
            // consumer-side fault: rewind or fabricated head. Flag + reset
            // the ledger to a safe state; never UB, never silent.
            ledger_fault_reset(blk, cr_head_obs);
            return;
        }
        unretired -= span;
        head = head.wrapping_add(1);
    }
    write_u32(blk + BK_UNRETIRED_OFF, unretired);
    write_u32(blk + BK_LEDGER_HEAD_OFF, head);
    write_u32(blk + BK_LAST_CRH_OFF, cr_head_obs);
}

/// Complete a MODE_ASYNC request into the completion ring (§12): write the
/// CompletionRecord fields, push the RX-ledger span, then SeqCst-publish
/// `cr_tail`, then bump `cr_seq`. The caller has verified ring space (the
/// §17.2 gate). Returns true if `cr_seq` must be notified.
unsafe fn complete_async(
    ep_base: u32,
    blk: u32,
    req_id: u64,
    op: u32,
    out: &Outcome,
    result_off: u32,
) -> bool {
    let crh = ep_base + EP_COMPLETION_RING_HEADER_OFF;
    let tail = au32(crh + RH_TAIL).load(Ordering::SeqCst);
    let slot = ep_ring_slot(ep_base, EP_COMPLETION_SLOTS_OFF, tail);

    // Fields first; the cr_tail store publishes them (§15.4).
    write_u64(slot + CREC_REQ_ID, req_id);
    write_u32(slot + CREC_OP, op);
    write_u32(slot + CREC_STATUS, out.status as u32);
    write_u32(slot + CREC_KIND, out.kind);
    write_u32(slot + CREC_LEN, out.len);
    write_u32(slot + CREC_OFF, result_off);
    write_u32(slot + CREC_CFLAGS, 0);
    write_bytes(slot + CREC_INLINE, &out.inline_);

    // RX ledger push (§9.3 per-record byte ledger). rx_tail was advanced by
    // rx_alloc at dispatch; unretired grows by the span here.
    if out.rx_span > 0 {
        let unretired = read_u32(blk + BK_UNRETIRED_OFF).wrapping_add(out.rx_span);
        write_u32(blk + BK_UNRETIRED_OFF, unretired);
    }
    write_u32(blk + BK_LENS_OFF + (tail & RING_INDEX_MASK) * 4, out.rx_span);

    au32(crh + RH_TAIL).store(tail.wrapping_add(1), Ordering::SeqCst);
    au32(crh + RH_EXTRA).fetch_add(1, Ordering::SeqCst); // cr_seq doorbell
    let done = read_u64(blk + BK_COMPLETED_OFF) + 1;
    write_u64(blk + BK_COMPLETED_OFF, done);
    true
}

// ---------------------------------------------------------------------------
// the pump (ABI §10; P2-004 four-lane redesign — P2_004_CONCURRENCY.md §3)
// ---------------------------------------------------------------------------

/// One kernel pass. Lanes per endpoint:
///   (0) sync priority scan (§10.3) — never deferred (§11.7/§16.2);
///   (1) cancel pre-scan (§18/§29 — D's normative reading);
///   (2) async FIFO with the §17.2 deferral gate + §17.3-item-4 EAGAIN
///       conversion (ASYNC_DEFER_LIMIT = 40);
///   (3) contiguous-consumed-prefix `sr_head` advance (§10.3 — the P2-003
///       per-entry-store bug is structurally fixed here).
///
/// `op_budget` bounds the number of requests processed per call (0 =
/// unbounded): the driver re-pumps while `nodine_pending()` reports work,
/// keeping the §5.3 heartbeat fresh under full saturation (F-E1).
///
/// Returns the u64 ASYNC notify mask (bit i = endpoint i produced async
/// completions this pass → the driver notifies `cr_seq`). Sync completions
/// are reported via the separate per-pass sync mask (`nodine_sync_mask`,
/// KH word) → the driver notifies `sm_seq` only for those endpoints. This
/// split prevents sync traffic from spuriously waking async waiter chains.
/// Kernel-private, never blocks, never waits (§10.5).
#[no_mangle]
pub unsafe extern "C" fn nodine_pump(now_ms: u64, op_budget: u32) -> u64 {
    au64(KCTL_HEARTBEAT).store(now_ms, Ordering::SeqCst);

    if !check_canaries() {
        mark_corrupt();
        core::arch::wasm32::unreachable(); // fail loudly (§19.1)
    }

    let state = au32(KCTL_KERNEL_STATE).load(Ordering::SeqCst);
    if state != KERNEL_STATE_RUNNING {
        write_u32(KH_PENDING_OFF, 0);
        return 0; // BOOTING (nothing to do), DRAINING/STOPPED (shutdown owns the sweep)
    }

    let mut notify_mask: u64 = 0;
    let mut pending: u64 = 0;
    write_u64(KH_SYNC_MASK_OFF, 0); // per-pass sync-activity mask (driver uses
                                    // it to notify sm_seq ONLY for endpoints
                                    // with sync completions — a sync op must
                                    // not spuriously wake async waiters)
    // budget 0 = unbounded: use a sentinel that cannot be exhausted in a
    // single pass (a pass is bounded by ring capacity × endpoint count).
    let mut budget: u32 = if op_budget == 0 { u32::MAX } else { op_budget };

    let mut ep_index: u32 = 0;
    while ep_index < MAX_ENDPOINTS {
        let slot = ENDPOINT_TABLE_OFFSET + ep_index * 64;
        let ep_state = au32(slot + EPS_STATE).load(Ordering::SeqCst);
        if ep_state != ENDPOINT_STATE_RUNNING && ep_state != ENDPOINT_STATE_ALLOCATED {
            ep_index += 1;
            continue;
        }
        let ep_base = au32(slot + EPS_EP_BASE).load(Ordering::SeqCst);
        let dir = EPDIR_BASE + ep_index * EPDIR_STRIDE;
        let blk = read_u32(dir + EPD_BLOCK_OFF);
        if blk == 0 {
            ep_index += 1;
            continue; // no kernel state (defensive; cannot happen)
        }

        let result = pump_endpoint(ep_index, ep_base, blk, ep_state, &mut budget, &mut notify_mask);
        if result {
            pending |= 1u64 << ep_index;
        }
        if op_budget != 0 && budget == 0 && ep_index + 1 < MAX_ENDPOINTS {
            // budget exhausted: mark remaining endpoints with work as pending
            // (cheap conservative scan of ring occupancy) and return.
            let mut j = ep_index; // re-check current too (partial processing)
            while j < MAX_ENDPOINTS {
                let s2 = ENDPOINT_TABLE_OFFSET + j * 64;
                let st2 = au32(s2 + EPS_STATE).load(Ordering::SeqCst);
                if st2 == ENDPOINT_STATE_RUNNING || st2 == ENDPOINT_STATE_ALLOCATED {
                    let b2 = au32(s2 + EPS_EP_BASE).load(Ordering::SeqCst);
                    let srh2 = b2 + EP_SUBMIT_RING_HEADER_OFF;
                    let used2 = au32(srh2 + RH_TAIL).load(Ordering::SeqCst)
                        .wrapping_sub(au32(srh2 + RH_HEAD).load(Ordering::SeqCst));
                    if used2 > 0 {
                        pending |= 1u64 << j;
                    }
                }
                j += 1;
            }
            break;
        }
        ep_index += 1;
    }

    write_u32(KH_PENDING_OFF, pending as u32);
    write_u32(KH_PENDING_OFF + 4, (pending >> 32) as u32);
    notify_mask
}

/// Process one endpoint's lanes. Returns true if unconsumed work remains
/// (deferred or budget-exhausted).
unsafe fn pump_endpoint(
    ep_index: u32,
    ep_base: u32,
    blk: u32,
    ep_state: u32, // pre-pass snapshot — the C1 gate re-reads the LIVE slot state
    budget: &mut u32,
    notify_mask: &mut u64,
) -> bool {
    let srh = ep_base + EP_SUBMIT_RING_HEADER_OFF;
    let crh = ep_base + EP_COMPLETION_RING_HEADER_OFF;
    let budget_at_entry = *budget;

    // Retire ledger pops up to the consumer's observed cr_head (§9.3) —
    // before any gate decision, so freed RX space is visible this pass.
    let cr_head_obs = au32(crh + RH_HEAD).load(Ordering::SeqCst);
    // ADR-0012: a cr-head advance since the last pop means the bridge
    // retired RX spans this pass — the consumer is demonstrably ALIVE. Such
    // a pass can never count toward the ASYNC_DEFER_LIMIT starvation ladder
    // even if the endpoint itself made no progress (arena still short): the
    // ladder counts STARVATION (a wedged consumer), not
    // backpressure-with-progress. The arena's contiguous retire-order
    // semantics (§9.3: alloc order = retire order) guarantee the deferred
    // head op completes once enough spans retire, so an alive consumer
    // means liveness holds and the kernel-authoritative EAGAIN conversion
    // (§17.3 item 4) must not fire.
    let consumer_retired = cr_head_obs != read_u32(blk + BK_LEDGER_HEAD_OFF);
    ledger_pop(blk, cr_head_obs);

    let mut head = au32(srh + RH_HEAD).load(Ordering::SeqCst);
    let tail = au32(srh + RH_TAIL).load(Ordering::SeqCst);
    let mut used = tail.wrapping_sub(head);
    if used == 0 {
        return false;
    }
    if used > RING_SLOTS {
        // Hostile/buggy producer published past ring capacity: its tail
        // wrapped over unconsumed slots — the overwritten prefix
        // [head, tail−RING_SLOTS) is unrecoverable. Recover the endpoint
        // instead of wedging it forever: drop the corrupted prefix, count a
        // protocol diagnostic, process the valid suffix (§19.1 explicit /
        // §19.4 hostile-producer defense). A sync entry lost this way
        // surfaces to its worker via the §11.3 ladder (bounded, explicit).
        let new_head = tail.wrapping_sub(RING_SLOTS);
        au32(srh + RH_HEAD).store(new_head, Ordering::SeqCst);
        let f = read_u32(KH_FLAGS_OFF);
        write_u32(KH_FLAGS_OFF, f | KH_FLAG_RING_FAULT);
        head = new_head;
        used = RING_SLOTS;
    }

    // ---- LANE 0: sync priority scan (§10.3) ------------------------------
    let sync_pending = au32(srh + RH_EXTRA).load(Ordering::SeqCst);
    if sync_pending > 0 {
        let mut idx = head;
        let mut scanned: u32 = 0;
        while scanned < used {
            let s = ep_ring_slot(ep_base, EP_SUBMIT_SLOTS_OFF, idx);
            if !get_bit(blk + BK_CONSUMED_OFF, idx & RING_INDEX_MASK)
                && read_u32(s + REQ_FLAGS) & FLAG_MODE_SYNC != 0
            {
                let is_sync = true;
                let outcome = process_entry(
                    ep_index, ep_base, blk, ep_state, s, idx, tail, is_sync, crh, budget, notify_mask,
                );
                if outcome {
                    // processed (consumed mark set inside); decrement the hint
                    au32(srh + RH_EXTRA).fetch_sub(1, Ordering::SeqCst);
                }
                if *budget == 0 {
                    // budget exhausted mid-lane
                    let mut adv = head;
                    while adv != tail && get_bit(blk + BK_CONSUMED_OFF, adv & RING_INDEX_MASK) {
                        clear_bit(blk + BK_CONSUMED_OFF, adv & RING_INDEX_MASK);
                        adv = adv.wrapping_add(1);
                    }
                    au32(srh + RH_HEAD).store(adv, Ordering::SeqCst);
                    return true;
                }
            }
            idx = idx.wrapping_add(1);
            scanned += 1;
        }
    }

    // ---- LANE 1: cancel pre-scan (§18/§29, ADR-0010 D4) -------------------
    // Resolve visible async OP_CANCELs: set cancel-pending on the first
    // unconsumed slot whose req_id matches the target. Effects at scan time;
    // completions are emitted at each slot's FIFO position in lane 2
    // (preserves §12.4.2 + §12.4.5).
    {
        let mut idx = head;
        let mut scanned: u32 = 0;
        while scanned < used {
            let s = ep_ring_slot(ep_base, EP_SUBMIT_SLOTS_OFF, idx);
            if !get_bit(blk + BK_CONSUMED_OFF, idx & RING_INDEX_MASK)
                && read_u32(s + REQ_OP) == Op::Cancel as u32
                && read_u32(s + REQ_FLAGS) & FLAG_MODE_SYNC == 0
            {
                let target = read_u64(s + REQ_A0);
                // first-slot match over the unconsumed window (D: the ring
                // slots ARE the outstanding table)
                let mut j = head;
                let mut sc2: u32 = 0;
                while sc2 < used {
                    if j != idx {
                        let t = ep_ring_slot(ep_base, EP_SUBMIT_SLOTS_OFF, j);
                        if !get_bit(blk + BK_CONSUMED_OFF, j & RING_INDEX_MASK)
                            && read_u64(t + REQ_REQ_ID) == target
                        {
                            set_bit(blk + BK_CANCEL_OFF, j & RING_INDEX_MASK);
                            break; // first match wins; duplicates no-op
                        }
                    }
                    j = j.wrapping_add(1);
                    sc2 += 1;
                }
                // not found: target already executed/completed (or stale id)
                // — the cancel completes status 0 (no-op) at lane 2.
            }
            idx = idx.wrapping_add(1);
            scanned += 1;
        }
    }

    // ---- LANE 2: async FIFO with the §17.2 gate ---------------------------
    // Two distinct stop reasons with different driver obligations:
    //   stopped_by_defer — the head-of-line op waits on consumer action
    //     (cr_ring space / RX retire): NOT immediately re-pumpable; recovery
    //     happens on the driver's ≤250 ms wait-timeout cadence (§10.5) or the
    //     next doorbell; the ASYNC_DEFER_LIMIT (§17.3) ladder converts it to
    //     EAGAIN after 40 passes.
    //   stopped_by_budget — op budget exhausted mid-lane: immediately
    //     re-pumpable (reported via the pending mask / return value).
    let mut stopped_by_defer = false;
    let mut stopped_by_budget = false;
    {
        let mut idx = head;
        while idx != tail {
            let bit_idx = idx & RING_INDEX_MASK;
            if get_bit(blk + BK_CONSUMED_OFF, bit_idx) {
                idx = idx.wrapping_add(1);
                continue;
            }
            if *budget == 0 {
                stopped_by_budget = true;
                break;
            }
            let s = ep_ring_slot(ep_base, EP_SUBMIT_SLOTS_OFF, idx);
            let flags = read_u32(s + REQ_FLAGS);
            if flags & FLAG_MODE_SYNC != 0 {
                // Sync entry visible but sr_sync_pending was 0 (bridge bug:
                // forgot the add). §10.3: "correctness never depends on it
                // (kernel scans when in doubt)" — process it as sync here.
                let outcome = process_entry(
                    ep_index, ep_base, blk, ep_state, s, idx, tail, true, crh, budget, notify_mask,
                );
                let _ = outcome;
                idx = idx.wrapping_add(1);
                continue;
            }

            let cr_head_now = au32(crh + RH_HEAD).load(Ordering::SeqCst);
            let cr_used = au32(crh + RH_TAIL).load(Ordering::SeqCst).wrapping_sub(cr_head_now);
            let op = read_u32(s + REQ_OP);

            if get_bit(blk + BK_CANCEL_OFF, bit_idx) {
                // target of a won cancel: ECANCELED at its FIFO position
                // (kind NONE — no ledger span; TX is producer-owned).
                if cr_used >= RING_SLOTS {
                    stopped_by_defer = true;
                    break;
                }
                let req_id = read_u64(s + REQ_REQ_ID);
                let out = Outcome::none(uv::ECANCELED);
                complete_async(ep_base, blk, req_id, op, &out, 0);
                *notify_mask |= 1u64 << ep_index;
                bump_stat(blk + BK_CANCELED_OFF);
                clear_bit(blk + BK_CANCEL_OFF, bit_idx);
                set_bit(blk + BK_CONSUMED_OFF, bit_idx);
                *budget -= 1;
                idx = idx.wrapping_add(1);
                continue;
            }

            if op == Op::Cancel as u32 {
                // the cancel's own completion: status 0 on both paths (§29)
                if cr_used >= RING_SLOTS {
                    stopped_by_defer = true;
                    break;
                }
                let req_id = read_u64(s + REQ_REQ_ID);
                let out = Outcome::none(0);
                complete_async(ep_base, blk, req_id, op, &out, 0);
                *notify_mask |= 1u64 << ep_index;
                set_bit(blk + BK_CONSUMED_OFF, bit_idx);
                *budget -= 1;
                idx = idx.wrapping_add(1);
                continue;
            }

            // normal async op: compute the deferral gate (§17.2/§9.3). The
            // RX check uses the CONTIGUOUS-free allocator probe (§6.2 linear
            // records) plus the 64-byte headroom of §9.3. P2-005-006: the
            // fs payload ops joined the gate — READ (len_max), STAT/LSTAT/
            // FSTAT (144 B), READDIR (measured wire ≤ max_bytes).
            let mut need: u32 = 0;
            let mut wants_payload = false;
            if op == OP_TRANSPORT_PROBE {
                let len = read_u64(s + REQ_A0) as u32;
                if len > INLINE_CAP {
                    need = (len + 7) & !7;
                    wants_payload = true;
                }
            } else {
                match op {
                    0x0102 => {
                        // READ: area span = rounded len_max
                        let len_max = read_u64(s + REQ_A1) as u32;
                        if len_max > 0 {
                            need = (len_max + 7) & !7;
                            wants_payload = true;
                        }
                    }
                    0x0104 | 0x0105 | 0x0106 => {
                        // STAT/LSTAT/FSTAT: 144 B (area span; errors carry
                        // none — deferring an ENOENT is latency, not failure)
                        need = 144;
                        wants_payload = true;
                    }
                    0x010B => {
                        // READDIR: P2-008 Step-3 reliability fix — the
                        // worst-case area span is the caller's max_bytes
                        // bound, but the MEASURED batch is usually far
                        // smaller (a few hundred bytes vs the typical 64 KiB
                        // npm bound). Probing at max_bytes made the §17.2
                        // gate defer an op whose real span would fit, and the
                        // §17.3-item-4 ladder then converted a VALID READDIR
                        // to EAGAIN under arena fragmentation (observed live:
                        // npm ci rm-storm, 1 of 9 executions). The gate now
                        // probes the PROGRESS FLOOR — the forced-first-entry
                        // minimum the ABI already guarantees (validation
                        // floors max_bytes at 512 B) — and dispatch charges
                        // only the measured span (pass-1 measure → rx_alloc).
                        // If even the floor fits, dispatch is guaranteed to
                        // allocate: the emitted batch is ≥ floor only when
                        // the caller's max_bytes allows it, and rx_alloc
                        // re-probes at the measured size (structural ENOMEM
                        // stays the explicit §19.1 failure, never silent
                        // corruption). If the floor does NOT fit, the op
                        // defers as true backpressure — bounded by retire
                        // progress, with the ladder reserved for a genuinely
                        // wedged consumer.
                        let floor = 512u32;
                        need = floor;
                        wants_payload = true;
                    }
                    0x0126 => {
                        // READLINK (P2-006-007): area span = rounded len_max
                        let len_max = read_u64(s + REQ_A0) as u32;
                        if len_max > 0 {
                            need = (len_max + 7) & !7;
                            wants_payload = true;
                        }
                    }
                    _ => {}
                }
            }
            let rx_short = wants_payload && match rx_alloc_probe(blk, need + RESULT_AREA_HEADROOM) {
                Some(_) => false,
                None => true,
            };
            let defer_passes = read_u32(blk + BK_DEFER_OFF);

            if cr_used >= RING_SLOTS || rx_short {
                if defer_passes >= ASYNC_DEFER_LIMIT && cr_used < RING_SLOTS {
                    // §17.3 item 4: kernel-authoritative EAGAIN conversion.
                    let req_id = read_u64(s + REQ_REQ_ID);
                    let out = Outcome::none(uv::EAGAIN);
                    complete_async(ep_base, blk, req_id, op, &out, 0);
                    *notify_mask |= 1u64 << ep_index;
                    bump_stat(blk + BK_EAGAIN_OFF);
                    set_bit(blk + BK_CONSUMED_OFF, bit_idx);
                    write_u32(blk + BK_DEFER_OFF, 0);
                    *budget -= 1;
                    idx = idx.wrapping_add(1);
                    continue;
                }
                stopped_by_defer = true;
                break; // head-of-line deferral (§17.2/§22.4)
            }

            let outcome = process_entry(
                ep_index, ep_base, blk, ep_state, s, idx, tail, false, crh, budget, notify_mask,
            );
            let _ = outcome;
            idx = idx.wrapping_add(1);
        }
    }

    // Deferral bookkeeping (§17.3, ADR-0012 true-starvation semantics): the
    // ASYNC_DEFER_LIMIT ladder counts passes where the endpoint made NO
    // progress AND the consumer retired NOTHING (cr head frozen) — i.e. a
    // genuinely wedged consumer. A pass where the consumer demonstrably
    // retired spans resets the ladder even without endpoint progress: the
    // freed arena space guarantees the deferred head op completes on a
    // later pass (progress is bounded by retire order, not by luck), so
    // EAGAIN must not fire. Delayed-but-alive consumers (drain turns queued
    // behind submit traffic on a loaded box) no longer climb the ladder.
    let processed = budget_at_entry.wrapping_sub(*budget);
    if stopped_by_defer {
        if processed == 0 && !consumer_retired {
            let d = read_u32(blk + BK_DEFER_OFF);
            if d < 0xFFFF_FFFF {
                write_u32(blk + BK_DEFER_OFF, d + 1);
            }
        } else {
            write_u32(blk + BK_DEFER_OFF, 0); // progress, or consumer alive ⇒ not starvation
        }
    } else if !stopped_by_budget {
        // window fully processed (or empty) without deferral
        write_u32(blk + BK_DEFER_OFF, 0);
    }

    // ---- LANE 3: contiguous-consumed-prefix head advance (§10.3) ---------
    let mut adv = head;
    while adv != tail && get_bit(blk + BK_CONSUMED_OFF, adv & RING_INDEX_MASK) {
        clear_bit(blk + BK_CONSUMED_OFF, adv & RING_INDEX_MASK); // ready for next life
        adv = adv.wrapping_add(1);
    }
    au32(srh + RH_HEAD).store(adv, Ordering::SeqCst);

    // Pending = immediately re-pumpable (budget-exhausted). A deferred
    // endpoint is NOT pending: it waits on consumer action; the driver's
    // ≤250 ms cadence (§10.5) re-checks it and the §17.3 ladder eventually
    // converts it to EAGAIN.
    stopped_by_budget
}

/// OR the endpoint's bit into the per-pass sync-activity mask (KH word). The
/// driver notifies `sm_seq` ONLY for these endpoints and `cr_seq` only for
/// the pump's returned async mask — a sync op must not spuriously wake the
/// client's async waiter chain (measured as real contention on 2-core boxes).
#[inline]
unsafe fn sync_mask_or(ep_index: u32) {
    if ep_index < 64 {
        let m = read_u64(KH_SYNC_MASK_OFF);
        write_u64(KH_SYNC_MASK_OFF, m | (1u64 << ep_index));
    }
}

/// Per-pass sync-activity mask (driver-side notify routing).
#[no_mangle]
pub unsafe extern "C" fn nodine_sync_mask() -> u64 {
    read_u64(KH_SYNC_MASK_OFF)
}

#[inline]
unsafe fn bump_stat(off: u32) {
    let v = read_u64(off) + 1;
    write_u64(off, v);
}

/// Validate + dispatch + complete one entry (sync or async). Marks the slot
/// consumed on every terminal path. Returns true if consumed.
unsafe fn process_entry(
    ep_index: u32,
    ep_base: u32,
    blk: u32,
    ep_state: u32,
    s: u32,
    idx: u32,
    tail_at_start: u32,
    is_sync: bool,
    crh: u32,
    budget: &mut u32,
    notify_mask: &mut u64,
) -> bool {
    let _ = crh;
    let req = match validate_request(s, is_sync, ep_base) {
        Ok(r) => r,
        Err(status) => {
            let req = RawRequest {
                req_id: read_u64(s + REQ_REQ_ID),
                op: read_u32(s + REQ_OP),
                flags: read_u32(s + REQ_FLAGS),
                tx_off: read_u32(s + REQ_TX_OFF),
                tx_len: read_u32(s + REQ_TX_LEN),
                a0: read_u64(s + REQ_A0),
                a1: 0,
                a2: 0,
            };
            let out = Outcome::none(status);
            if is_sync {
                if complete_sync(ep_base, &req, &out) {
                    sync_mask_or(ep_index);
                }
            } else {
                // §17.2 gate already passed; error completions fit (NONE).
                if complete_async(ep_base, blk, req.req_id, req.op, &out, 0) {
                    *notify_mask |= 1u64 << ep_index;
                }
            }
            set_bit(blk + BK_CONSUMED_OFF, idx & RING_INDEX_MASK);
            *budget -= 1;
            return true;
        }
    };

    let _ = ep_state; // superseded by live_state below (C1)
    // Endpoint-state gate (C1 HARDENING, AV-FD-01): re-read the SLOT state —
    // lane 2 can reach this dispatch after an in-pass endpoint_drain flipped
    // the slot DEAD. Only a RUNNING slot dispatches; an ALLOCATED slot takes
    // only OP_PROC_READY (§7.2); DEAD/DRAINING/FREE ⇒ EENDPOINT with ZERO
    // side effects (also defines OP_OPEN-from-a-recordless-caller).
    let live_state = au32(ENDPOINT_TABLE_OFFSET + ep_index * 64 + EPS_STATE).load(Ordering::SeqCst);
    let dispatchable = if live_state == ENDPOINT_STATE_RUNNING {
        true
    } else if live_state == ENDPOINT_STATE_ALLOCATED {
        req.op == Op::ProcReady as u32
    } else {
        false // DEAD / DRAINING / FREE — never dispatch (AV-FD-01)
    };
    if !dispatchable {
        let out = Outcome::none(NODINE_EENDPOINT);
        if is_sync {
            if complete_sync(ep_base, &req, &out) { sync_mask_or(ep_index); }
        } else {
            if complete_async(ep_base, blk, req.req_id, req.op, &out, 0) {
                *notify_mask |= 1u64 << ep_index;
            }
        }
        set_bit(blk + BK_CONSUMED_OFF, idx & RING_INDEX_MASK);
        *budget -= 1;
        return true;
    }

    // OP_PROC_READY: version re-check + ALLOCATED → RUNNING (§7.2).
    if req.op == Op::ProcReady as u32 {
        let ok = abi_compatible(ABI_VERSION, req.a0 as u32);
        let slot = ENDPOINT_TABLE_OFFSET + ep_index * 64;
        // P2-005-005 (FD_SPEC §8): process birth — create-if-absent keyed by
        // the SLOT's pid (never the endpoint index), gated on the ingress
        // caps bit (FC-11: an ingress endpoint mints NO ProcessRecord — it
        // is a kernel-internal proxy, not a process).
        let mut birth_err: i32 = 0;
        if ok {
            let pid = au32(slot + EPS_PID).load(Ordering::SeqCst);
            let caps = au32(slot + EPS_CAPS).load(Ordering::SeqCst);
            if pid != 0 && caps & ENDPOINT_CAP_INGRESS == 0 {
                match (unsafe { fdstate_mut() }, unsafe { vfs_mut() }) {
                    (Some(fd), Some(vfs)) => {
                        if fd.process_birth(vfs, pid, kernel_vfs::FD_DEFAULT_UMASK).is_err() {
                            birth_err = uv::ENOMEM; // explicit completion (FC-5)
                        }
                    }
                    _ => {} // FD subsystem not booted — nothing to mint
                }
            }
        }
        let out = if !ok {
            Outcome::none(NODINE_EABI_VERSION)
        } else if birth_err != 0 {
            Outcome::none(birth_err)
        } else {
            let st = au32(slot + EPS_STATE).load(Ordering::SeqCst);
            if st == ENDPOINT_STATE_ALLOCATED {
                au32(ep_base + EPH_STATE).store(ENDPOINT_STATE_RUNNING, Ordering::SeqCst);
                au32(slot + EPS_STATE).store(ENDPOINT_STATE_RUNNING, Ordering::SeqCst);
            }
            Outcome::none(0)
        };
        if is_sync {
            if complete_sync(ep_base, &req, &out) { sync_mask_or(ep_index); }
        } else {
            if complete_async(ep_base, blk, req.req_id, req.op, &out, 0) {
                *notify_mask |= 1u64 << ep_index;
            }
        }
        set_bit(blk + BK_CONSUMED_OFF, idx & RING_INDEX_MASK);
        *budget -= 1;
        return true;
    }

    // OP_PROC_EXIT: the §21.2 drain, executed at this entry's position.
    if req.op == Op::ProcExit as u32 {
        // Complete PROC_EXIT itself first (status 0), then sweep the rest.
        let out = Outcome::none(0);
        if is_sync {
            if complete_sync(ep_base, &req, &out) { sync_mask_or(ep_index); }
        } else {
            if complete_async(ep_base, blk, req.req_id, req.op, &out, 0) {
                *notify_mask |= 1u64 << ep_index;
            }
        }
        set_bit(blk + BK_CONSUMED_OFF, idx & RING_INDEX_MASK);
        *budget -= 1;
        endpoint_drain(ep_index, ep_base, blk, notify_mask);
        return true;
    }

    // OP_TRANSPORT_PROBE: endpoint-aware dispatch (work loop + areas).
    if req.op == OP_TRANSPORT_PROBE {
        let srh = ep_base + EP_SUBMIT_RING_HEADER_OFF;
        let out = dispatch_transport_probe(&req, is_sync, ep_base, blk, srh, tail_at_start);
        if is_sync {
            if complete_sync(ep_base, &req, &out) {
                sync_mask_or(ep_index);
            }
        } else {
            if complete_async(ep_base, blk, req.req_id, req.op, &out, out.rx_off) {
                *notify_mask |= 1u64 << ep_index;
            }
            if out.aborted {
                bump_stat(blk + BK_CANCELED_OFF);
            }
        }
        set_bit(blk + BK_CONSUMED_OFF, idx & RING_INDEX_MASK);
        *budget -= 1;
        return true;
    }

    // Filesystem v1 surface (P2-005-006): OP_OPEN..OP_FTRUNCATE (0x0100
    // through 0x010C) execute against the REAL FdState + Vfs. P2-006-007
    // extends the live set with OP_PROC_CHDIR (0x0015 — process range,
    // PATH payload) and the fs deferred surface OP_TRUNCATE/CHMOD/UTIMES/
    // LINK/SYMLINK/READLINK (0x0121-0x0126). 0x010D (OP_ACCESS) and the
    // 0x0110+ watch stubs still fall through to the explicit ENOOP
    // completion below.
    if (0x0100..=0x010C).contains(&req.op)
        || req.op == 0x0015
        || (0x0121..=0x0126).contains(&req.op)
    {
        let out = dispatch_fs(&req, ep_index, ep_base, blk, is_sync);
        if is_sync {
            if complete_sync(ep_base, &req, &out) {
                sync_mask_or(ep_index);
            }
        } else {
            if complete_async(ep_base, blk, req.req_id, req.op, &out, out.rx_off) {
                *notify_mask |= 1u64 << ep_index;
            }
        }
        set_bit(blk + BK_CONSUMED_OFF, idx & RING_INDEX_MASK);
        *budget -= 1;
        return true;
    }

    // Everything else: explicit stub / control completion.
    let out = dispatch(&req);
    if is_sync {
        if complete_sync(ep_base, &req, &out) {
            sync_mask_or(ep_index);
        }
    } else {
        if complete_async(ep_base, blk, req.req_id, req.op, &out, 0) {
            *notify_mask |= 1u64 << ep_index;
        }
    }
    set_bit(blk + BK_CONSUMED_OFF, idx & RING_INDEX_MASK);
    *budget -= 1;
    true
}

/// §21.2 process drain: fail every not-yet-dequeued request with ECANCELED
/// (ring order), reclaim transport state, mark the endpoint DEAD. Best-effort
/// on completion-ring space (a full ring defers the sweep; the exiting
/// process's consumer is going away — the endpoint goes DEAD regardless and
/// the region is re-initialized on recycle).
unsafe fn endpoint_drain(ep_index: u32, ep_base: u32, blk: u32, notify_mask: &mut u64) {
    let slot = ENDPOINT_TABLE_OFFSET + ep_index * 64;
    au32(ep_base + EPH_STATE).store(ENDPOINT_STATE_DRAINING, Ordering::SeqCst);
    au32(slot + EPS_STATE).store(ENDPOINT_STATE_DRAINING, Ordering::SeqCst);

    let srh = ep_base + EP_SUBMIT_RING_HEADER_OFF;
    let crh = ep_base + EP_COMPLETION_RING_HEADER_OFF;
    let head = au32(srh + RH_HEAD).load(Ordering::SeqCst);
    let tail = au32(srh + RH_TAIL).load(Ordering::SeqCst);
    let used = tail.wrapping_sub(head);
    if used <= RING_SLOTS {
        let mut idx = head;
        let mut scanned: u32 = 0;
        while scanned < used {
            let bit_idx = idx & RING_INDEX_MASK;
            if !get_bit(blk + BK_CONSUMED_OFF, bit_idx) {
                let s = ep_ring_slot(ep_base, EP_SUBMIT_SLOTS_OFF, idx);
                let is_sync = read_u32(s + REQ_FLAGS) & FLAG_MODE_SYNC != 0;
                let req_id = read_u64(s + REQ_REQ_ID);
                let op = read_u32(s + REQ_OP);
                let out = Outcome::none(uv::ECANCELED);
                if is_sync {
                    let n = complete_sync(ep_base, &RawRequest {
                        req_id, op, flags: FLAG_MODE_SYNC, tx_off: 0, tx_len: 0, a0: 0, a1: 0, a2: 0,
                    }, &out);
                    if n {
                        sync_mask_or(ep_index);
                    }
                    set_bit(blk + BK_CONSUMED_OFF, bit_idx);
                    bump_stat(blk + BK_CANCELED_OFF);
                } else {
                    let cr_used = au32(crh + RH_TAIL).load(Ordering::SeqCst)
                        .wrapping_sub(au32(crh + RH_HEAD).load(Ordering::SeqCst));
                    if cr_used >= RING_SLOTS {
                        break; // ring full: remaining entries die with the endpoint
                    }
                    complete_async(ep_base, blk, req_id, op, &out, 0);
                    *notify_mask |= 1u64 << ep_index;
                    bump_stat(blk + BK_CANCELED_OFF);
                    set_bit(blk + BK_CONSUMED_OFF, bit_idx);
                }
            }
            idx = idx.wrapping_add(1);
            scanned += 1;
        }
    }
    // advance head over everything consumed; dead requests stay unconsumed
    // (harmless — the endpoint is DEAD and its region is re-initialized on
    // recycle by endpoint_region_init).
    let mut adv = head;
    while adv != tail && get_bit(blk + BK_CONSUMED_OFF, adv & RING_INDEX_MASK) {
        clear_bit(blk + BK_CONSUMED_OFF, adv & RING_INDEX_MASK);
        adv = adv.wrapping_add(1);
    }
    au32(srh + RH_HEAD).store(adv, Ordering::SeqCst);

    // Reclaim: reset dynamic transport state; preserve lifetime stats.
    // P2-005-005 (FC-2): the shared fd/OFD/cwd reclaim runs BEFORE the DEAD
    // store (pid is read from the slot inside the helper).
    unsafe { fd_reclaim_process(ep_index) };
    block_reset_dynamic(blk);
    au32(ep_base + EPH_STATE).store(ENDPOINT_STATE_DEAD, Ordering::SeqCst);
    au32(slot + EPS_STATE).store(ENDPOINT_STATE_DEAD, Ordering::SeqCst);
    let dir = EPDIR_BASE + ep_index * EPDIR_STRIDE;
    let deaths = read_u64(dir + EPD_DEATHS_OFF) + 1;
    write_u64(dir + EPD_DEATHS_OFF, deaths);
}

// ---------------------------------------------------------------------------
// driver control-plane exports (kernel-internal surface, NOT ABI ops)
// ---------------------------------------------------------------------------

/// Endpoints with unconsumed or deferred work after the last pump call.
#[no_mangle]
pub unsafe extern "C" fn nodine_pending() -> u64 {
    (read_u32(KH_PENDING_OFF) as u64) | ((read_u32(KH_PENDING_OFF + 4) as u64) << 32)
}

/// Allocate an endpoint for `pid` with `caps` (ENDPOINT_CAP_*). The slot gets
/// a fresh generation; the region is fully re-initialized (a recycled
/// endpoint never inherits state — ADR-0003). Returns the endpoint index or
/// a negative errno (ENFILE: table full; ENOMEM: kernel heap exhausted).
#[no_mangle]
pub unsafe extern "C" fn nodine_alloc_endpoint(pid: u32, caps: u32) -> i32 {
    let state = au32(KCTL_KERNEL_STATE).load(Ordering::SeqCst);
    if state != KERNEL_STATE_RUNNING {
        return NODINE_ESHUTDOWN;
    }
    // P2-005-005 (FC-11, AV-FD-11): pid validation — pid 0 is the kernel
    // namespace (reserved for ingress-class endpoints), and a live pid may
    // never be re-issued (ABI §8.2 never-reuse gets a kernel backstop
    // instead of host trust; ≤64 records — a linear scan is free).
    if (caps & ENDPOINT_CAP_INGRESS) == 0 {
        if pid == 0 {
            return NODINE_EINVAL_REQUEST;
        }
        if let Some(fd) = unsafe { fdstate_mut() } {
            if fd.process_exists(pid) {
                return NODINE_EINVAL_REQUEST; // live pid — never re-issued
            }
        }
    }
    let mut i: u32 = 0;
    while i < MAX_ENDPOINTS {
        let slot = ENDPOINT_TABLE_OFFSET + i * 64;
        let st = au32(slot + EPS_STATE).load(Ordering::SeqCst);
        // §21.2(f)/ADR-0003: DEAD endpoints are reclaimed and RECYCLABLE — a
        // recycled slot gets a fresh generation (stale bridge views fail
        // their §20.2 step-3 check). FREE slots are first allocations.
        if st == ENDPOINT_STATE_FREE || st == ENDPOINT_STATE_DEAD {
            let dir = EPDIR_BASE + i * EPDIR_STRIDE;
            let mut blk = read_u32(dir + EPD_BLOCK_OFF);
            if blk == 0 {
                // P2-005-004b: fixed slot (mapped at boot with the whole block
                // region — this cannot fail in practice; the check stays).
                blk = ep_block_addr(i);
                if !ensure_mapped(blk + BK_SIZE) {
                    return uv::ENOMEM;
                }
                zero_bytes(blk, BK_SIZE);
                write_u64(blk + BK_MAGIC_OFF, BK_MAGIC);
                write_u64(blk + BK_CANARY_OFF, BK_CANARY);
                write_u32(dir + EPD_BLOCK_OFF, blk);
            } else {
                block_reset_dynamic(blk);
                zero_bytes(blk + BK_COMPLETED_OFF, 0x20); // reset 4 stat counters
            }
            let gen = read_u32(dir + EPD_GEN_OFF).wrapping_add(1);
            write_u32(dir + EPD_GEN_OFF, gen);
            write_u32(dir + EPD_FLAGS_OFF, EPD_FLAG_ALLOCATED);

            endpoint_region_init(i, pid, gen, caps);
            au64(slot + EPS_MAGIC).store(ENDPOINT_SLOT_MAGIC, Ordering::SeqCst);
            au32(slot + EPS_EP_BASE).store(TRANSPORT_BASE.wrapping_add(i.wrapping_mul(EP_STRIDE)), Ordering::SeqCst);
            au32(slot + EPS_PID).store(pid, Ordering::SeqCst);
            au32(slot + EPS_GENERATION).store(gen, Ordering::SeqCst);
            au32(slot + EPS_CAPS).store(caps, Ordering::SeqCst);
            au32(slot + EPS_STATE).store(ENDPOINT_STATE_ALLOCATED, Ordering::SeqCst); // last (§5.4)
            return i as i32;
        }
        i += 1;
    }
    uv::ENFILE
}

/// Clean process exit (§21.2): the driver tells the kernel the worker exited
/// normally (or the OP_PROC_EXIT op path invokes it internally).
#[no_mangle]
pub unsafe extern "C" fn nodine_endpoint_exit(ep_index: u32, generation: u32) -> i32 {
    if ep_index >= MAX_ENDPOINTS {
        return NODINE_EINVAL_REQUEST;
    }
    let slot = ENDPOINT_TABLE_OFFSET + ep_index * 64;
    let st = au32(slot + EPS_STATE).load(Ordering::SeqCst);
    if st != ENDPOINT_STATE_RUNNING && st != ENDPOINT_STATE_ALLOCATED {
        return NODINE_EENDPOINT;
    }
    let ep_base = au32(slot + EPS_EP_BASE).load(Ordering::SeqCst);
    let dir = EPDIR_BASE + ep_index * EPDIR_STRIDE;
    let blk = read_u32(dir + EPD_BLOCK_OFF);
    if blk == 0 {
        return NODINE_EENDPOINT;
    }
    // P2-005-005 (FC-2 rider, AV-FD-04): the clean-exit notice now takes a
    // GENERATION — a stale notice after a recycle must never drain (and
    // fd-reclaim) the slot's new occupant. (ABI-external control plane; the
    // signature change is kernel-internal, drivers updated with the records.)
    // NOTE: callers passing the CURRENT generation proceed; generation 0 is
    // accepted for the boot endpoint (pre-records compat) and rejected for
    // recycled slots (gen >= 2 there).
    let cur_gen = au32(slot + EPS_GENERATION).load(Ordering::SeqCst);
    if generation != 0 && generation != cur_gen {
        return NODINE_EENDPOINT; // stale exit notice
    }
    let mut mask: u64 = 0;
    endpoint_drain(ep_index, ep_base, blk, &mut mask);
    0
}

/// Crashed/terminated worker (§21.4 item 2 — host-authoritative notice): the
/// same reclaim WITHOUT any ring writes ("ECANCELED toward nobody — the
/// consumer is gone"). A stale notice (generation mismatch) is rejected.
#[no_mangle]
pub unsafe extern "C" fn nodine_endpoint_died(ep_index: u32, generation: u32) -> i32 {
    if ep_index >= MAX_ENDPOINTS {
        return NODINE_EINVAL_REQUEST;
    }
    let slot = ENDPOINT_TABLE_OFFSET + ep_index * 64;
    let st = au32(slot + EPS_STATE).load(Ordering::SeqCst);
    if st != ENDPOINT_STATE_RUNNING && st != ENDPOINT_STATE_ALLOCATED {
        return NODINE_EENDPOINT;
    }
    let gen = au32(slot + EPS_GENERATION).load(Ordering::SeqCst);
    if gen != generation {
        return NODINE_EENDPOINT; // stale notice
    }
    let ep_base = au32(slot + EPS_EP_BASE).load(Ordering::SeqCst);
    let dir = EPDIR_BASE + ep_index * EPDIR_STRIDE;
    let blk = read_u32(dir + EPD_BLOCK_OFF);
    if blk == 0 {
        return NODINE_EENDPOINT;
    }
    // P2-005-005 (FC-2): the died path runs the SAME fd/OFD/cwd reclaim as
    // the drain (pid read before the DEAD store; no ring writes — the
    // consumer is gone, §21.4).
    unsafe { fd_reclaim_process(ep_index) };
    block_reset_dynamic(blk);
    au32(ep_base + EPH_STATE).store(ENDPOINT_STATE_DEAD, Ordering::SeqCst);
    au32(slot + EPS_STATE).store(ENDPOINT_STATE_DEAD, Ordering::SeqCst);
    let deaths = read_u64(dir + EPD_DEATHS_OFF) + 1;
    write_u64(dir + EPD_DEATHS_OFF, deaths);
    0
}

/// §21.4 item 3 heartbeat watchdog backstop: report RUNNING endpoints whose
/// bridge-written `ep_heartbeat_ms` is stale past `threshold_ms`. Marks (and
/// clears) the kernel-private SUSPECT flag; NEVER reclaims — only the host
/// decides termination (a blocked-but-alive worker cannot answer a ping).
#[no_mangle]
pub unsafe extern "C" fn nodine_watchdog(now_ms: u64, threshold_ms: u32) -> u64 {
    let mut mask: u64 = 0;
    let mut i: u32 = 0;
    while i < MAX_ENDPOINTS {
        let slot = ENDPOINT_TABLE_OFFSET + i * 64;
        if au32(slot + EPS_STATE).load(Ordering::SeqCst) == ENDPOINT_STATE_RUNNING {
            let ep_base = au32(slot + EPS_EP_BASE).load(Ordering::SeqCst);
            let hb = au64(ep_base + EPH_HEARTBEAT).load(Ordering::SeqCst);
            let dir = EPDIR_BASE + i * EPDIR_STRIDE;
            let stale = hb != 0 && now_ms.saturating_sub(hb) > threshold_ms as u64;
            let mut f = read_u32(dir + EPD_FLAGS_OFF);
            if stale {
                f |= EPD_FLAG_SUSPECT;
                mask |= 1u64 << i;
            } else {
                f &= !EPD_FLAG_SUSPECT;
            }
            write_u32(dir + EPD_FLAGS_OFF, f);
        }
        i += 1;
    }
    mask
}

/// Layout-opaque diagnostics for tests (field IDs stable, kernel-internal).
/// Fields 15–26 (P2-005-004b) and 27–33 (P2-005-005) are kernel-GLOBAL —
/// they bypass the endpoint check like fields 0/1:
///   15 kalloc region base · 16 kalloc region size · 17 kalloc live bytes
///   18 kalloc high-water · 19 (alloc_count<<32)|free_count
///   20 VFS alive (0/1) · 21 VFS mount count · 22 VFS live inodes
///   23 VFS epoch · 24 VFS mseq · 25 VFS residency B · 26 VFS slack B
///   27 OFD live · 28 OFD mints · 29 OFD destroys · 30 stdio-out bytes
///   31 stdio-out drops · 32 stdio-err bytes · 33 stdio-err drops
///   34 FD subsystem alive (0/1) · 35 live processes
#[no_mangle]
pub unsafe extern "C" fn nodine_diag(ep_index: u32, field: u32) -> i64 {
    if field == 0 {
        return ((KH_LAYOUT_VERSION as i64) << 32) | (KH_MAGIC as i64);
    }
    if field == 1 {
        return read_u32(KH_FLAGS_OFF) as i64;
    }
    // Kernel-global fields (P2-005-005 FD counters) — no endpoint needed.
    if (27..=37).contains(&field) {
        let Some(fd) = (unsafe { fdstate_mut() }) else {
            return 0; // FD subsystem not booted
        };
        return match field {
            27..=33 => fd.diag(field),
            34 => 1,
            35 => fd.process_count() as i64,
            36 | 37 => fd.diag(field),
            _ => 0,
        };
    }
    // Kernel-global fields (P2-005-004b) — no endpoint needed.
    if (15..=26).contains(&field) {
        return match field {
            15 => read_u32(KH_KALLOC_OFF) as i64,
            16 => match kalloc_attach() {
                Some(_) => {
                    Kalloc::region_size(read_u32(KH_KALLOC_OFF) as *mut u8).unwrap_or(0) as i64
                }
                None => 0,
            },
            17 => match kalloc_attach() {
                Some(k) => k.live() as i64,
                None => 0,
            },
            18 => match kalloc_attach() {
                Some(k) => k.high_water() as i64,
                None => 0,
            },
            19 => match kalloc_attach() {
                Some(k) => {
                    let (a, f) = k.counters();
                    ((a as i64) << 32) | f as i64
                }
                None => 0,
            },
            20 => (read_u32(KH_VFS_OFF) != 0) as i64,
            21 => match vfs_mut() {
                Some(v) => v.mounts().len() as i64,
                None => 0,
            },
            22 => match vfs_mut() {
                Some(v) => v.accounting().inodes_live as i64,
                None => 0,
            },
            23 => match vfs_mut() {
                Some(v) => v.epoch() as i64,
                None => 0,
            },
            24 => match vfs_mut() {
                Some(v) => v.mseq() as i64,
                None => 0,
            },
            25 => match vfs_mut() {
                Some(v) => v.accounting().residency() as i64,
                None => 0,
            },
            26 => match vfs_mut() {
                Some(v) => {
                    let a = v.accounting();
                    (a.inode_bytes + a.dirent_bytes + a.dentry_index_bytes) as i64
                }
                None => 0,
            },
            _ => 0,
        };
    }
    if ep_index >= MAX_ENDPOINTS {
        return NODINE_EINVAL_REQUEST as i64;
    }
    let slot = ENDPOINT_TABLE_OFFSET + ep_index * 64;
    let ep_base = au32(slot + EPS_EP_BASE).load(Ordering::SeqCst);
    let dir = EPDIR_BASE + ep_index * EPDIR_STRIDE;
    let blk = read_u32(dir + EPD_BLOCK_OFF);
    if blk == 0 {
        return NODINE_EENDPOINT as i64;
    }
    match field {
        2 => read_u32(blk + BK_UNRETIRED_OFF) as i64,             // RX unretired bytes
        3 => read_u32(blk + BK_RX_TAIL_OFF) as i64,               // RX arena tail
        4 => read_u32(blk + BK_LEDGER_HEAD_OFF) as i64,           // ledger head
        5 => read_u32(blk + BK_DEFER_OFF) as i64,                 // deferral passes
        6 => read_u64(blk + BK_COMPLETED_OFF) as i64,             // completions (lifetime of block)
        7 => read_u64(blk + BK_CANCELED_OFF) as i64,              // ECANCELED count
        8 => read_u64(blk + BK_EAGAIN_OFF) as i64,                // EAGAIN count
        9 => read_u64(blk + BK_ESHUTDOWN_OFF) as i64,             // ESHUTDOWN count
        10 => read_u64(blk + BK_EXEC_ID_OFF) as i64,              // executing req_id
        11 => {                                                    // completion-ring used
            let crh = ep_base + EP_COMPLETION_RING_HEADER_OFF;
            au32(crh + RH_TAIL).load(Ordering::SeqCst)
                .wrapping_sub(au32(crh + RH_HEAD).load(Ordering::SeqCst)) as i64
        }
        12 => {                                                    // submit-ring used
            let srh = ep_base + EP_SUBMIT_RING_HEADER_OFF;
            au32(srh + RH_TAIL).load(Ordering::SeqCst)
                .wrapping_sub(au32(srh + RH_HEAD).load(Ordering::SeqCst)) as i64
        }
        13 => read_u64(dir + EPD_DEATHS_OFF) as i64,              // endpoint deaths
        14 => read_u32(dir + EPD_FLAGS_OFF) as i64,               // epdir flags
        _ => NODINE_EINVAL_REQUEST as i64,
    }
}

// ---------------------------------------------------------------------------
// shutdown (ABI §21.1 — complete graceful drain)
// ---------------------------------------------------------------------------

/// Graceful drain (§21.1): set DRAINING, then reject every not-yet-dequeued
/// request with a `NODINE_ESHUTDOWN` completion (best-effort on completion
/// -ring space — the spec guarantees inline error completions are always
/// writable, and a full ring's consumer is woken by the driver's final
/// §16.5 sweep and fails everything bridge-side), complete in-flight work
/// (none can be in flight on the serialized kernel), then publish STOPPED.
/// The driver performs the final ≤ MAX_ENDPOINTS×2 notify sweep (§21.1(e)).
#[no_mangle]
pub unsafe extern "C" fn nodine_shutdown(now_ms: u64) -> i32 {
    let state = au32(KCTL_KERNEL_STATE).load(Ordering::SeqCst);
    if state != KERNEL_STATE_RUNNING {
        return NODINE_EPROTOCOL;
    }
    au32(KCTL_KERNEL_STATE).store(KERNEL_STATE_DRAINING, Ordering::SeqCst);
    au64(KCTL_HEARTBEAT).store(now_ms, Ordering::SeqCst);

    // §21.1(a): ESHUTDOWN rejection sweep (re-read the tail so late arrivals
    // submitted before STOPPED are also rejected; bounded to 3 rounds).
    let mut round: u32 = 0;
    while round < 3 {
        let mut advanced = false;
        let mut i: u32 = 0;
        while i < MAX_ENDPOINTS {
            let slot = ENDPOINT_TABLE_OFFSET + i * 64;
            let st = au32(slot + EPS_STATE).load(Ordering::SeqCst);
            if st != ENDPOINT_STATE_RUNNING && st != ENDPOINT_STATE_ALLOCATED {
                i += 1;
                continue;
            }
            let ep_base = au32(slot + EPS_EP_BASE).load(Ordering::SeqCst);
            let dir = EPDIR_BASE + i * EPDIR_STRIDE;
            let blk = read_u32(dir + EPD_BLOCK_OFF);
            if blk == 0 {
                i += 1;
                continue;
            }
            let srh = ep_base + EP_SUBMIT_RING_HEADER_OFF;
            let crh = ep_base + EP_COMPLETION_RING_HEADER_OFF;
            ledger_pop(blk, au32(crh + RH_HEAD).load(Ordering::SeqCst));
            let head = au32(srh + RH_HEAD).load(Ordering::SeqCst);
            let mut tail = au32(srh + RH_TAIL).load(Ordering::SeqCst);
            let mut used = tail.wrapping_sub(head);
            if used == 0 || used > RING_SLOTS {
                i += 1;
                continue;
            }
            let mut idx = head;
            let mut scanned: u32 = 0;
            while scanned < used {
                let bit_idx = idx & RING_INDEX_MASK;
                if !get_bit(blk + BK_CONSUMED_OFF, bit_idx) {
                    let s = ep_ring_slot(ep_base, EP_SUBMIT_SLOTS_OFF, idx);
                    let is_sync = read_u32(s + REQ_FLAGS) & FLAG_MODE_SYNC != 0;
                    let req_id = read_u64(s + REQ_REQ_ID);
                    let op = read_u32(s + REQ_OP);
                    let out = Outcome::none(NODINE_ESHUTDOWN);
                    if is_sync {
                        if complete_sync(ep_base, &RawRequest {
                            req_id, op, flags: FLAG_MODE_SYNC, tx_off: 0, tx_len: 0, a0: 0, a1: 0, a2: 0,
                        }, &out) {
                            sync_mask_or(i);
                        }
                        set_bit(blk + BK_CONSUMED_OFF, bit_idx);
                        bump_stat(blk + BK_ESHUTDOWN_OFF);
                    } else {
                        let cr_used = au32(crh + RH_TAIL).load(Ordering::SeqCst)
                            .wrapping_sub(au32(crh + RH_HEAD).load(Ordering::SeqCst));
                        if cr_used >= RING_SLOTS {
                            break; // consumer must fail these bridge-side
                        }
                        complete_async(ep_base, blk, req_id, op, &out, 0);
                        set_bit(blk + BK_CONSUMED_OFF, bit_idx);
                        bump_stat(blk + BK_ESHUTDOWN_OFF);
                    }
                }
                idx = idx.wrapping_add(1);
                scanned += 1;
            }
            let mut adv = head;
            while adv != tail && get_bit(blk + BK_CONSUMED_OFF, adv & RING_INDEX_MASK) {
                clear_bit(blk + BK_CONSUMED_OFF, adv & RING_INDEX_MASK);
                adv = adv.wrapping_add(1);
            }
            if adv != au32(srh + RH_HEAD).load(Ordering::SeqCst) {
                advanced = true;
            }
            au32(srh + RH_HEAD).store(adv, Ordering::SeqCst);
            // late arrivals?
            let tail2 = au32(srh + RH_TAIL).load(Ordering::SeqCst);
            if tail2 != tail {
                advanced = true;
                let _ = used;
                tail = tail2;
                used = tail2.wrapping_sub(adv);
            }
            i += 1;
        }
        if !advanced {
            break;
        }
        round += 1;
    }

    au32(KCTL_KERNEL_STATE).store(KERNEL_STATE_STOPPED, Ordering::SeqCst);
    write_u32(KH_PENDING_OFF, 0);
    write_u32(KH_PENDING_OFF + 4, 0);
    // P2-005-005 (F OQ-6): global process reclaim — "zero live OFDs at stop"
    // becomes assertable by the smoke battery (FD_SPEC §8 shutdown row).
    {
        let mut i: u32 = 0;
        while i < MAX_ENDPOINTS {
            let slot = ENDPOINT_TABLE_OFFSET + i * 64;
            let st = au32(slot + EPS_STATE).load(Ordering::SeqCst);
            if st == ENDPOINT_STATE_RUNNING || st == ENDPOINT_STATE_ALLOCATED {
                unsafe { fd_reclaim_process(i) };
            }
            i += 1;
        }
    }
    // the driver's §16.5 final sweep notifies everything regardless; also
    // expose any last sync activity for completeness
    0
}

// ---------------------------------------------------------------------------
// P2-005-005: stdio sink drain (host-side console pump — FD_SPEC §4.3)
// ---------------------------------------------------------------------------

/// Copy up to `cap` bytes from the front of the stdio stream FIFO
/// (`stream` 0 = stdout, 1 = stderr) into the KSM at absolute offset
/// `dst_off`, removing them from the queue. Returns bytes moved (0 when
/// the FIFO is empty or the FD subsystem is not booted). The host driver
/// polls this on its regular pump cadence; overflow beyond the 256 KiB
/// FIFO was already dropped-with-counter at write time (never block, never
/// fail `console.log`).
#[no_mangle]
pub unsafe extern "C" fn nodine_stdio_drain(stream: u32, dst_off: u32, cap: u32) -> u32 {
    let Some(fd) = (unsafe { fdstate_mut() }) else {
        return 0;
    };
    if cap == 0 || cap > 0x1_0000 {
        return 0;
    }
    let cap = cap as usize;
    // Read into the KSM directly through a scratch slice (the KSM is the
    // module's own linear memory — a raw pointer write is the established
    // pattern; bounds were validated against KH_MEM_END at boot and the
    // driver passes in-KSM offsets only).
    let mut buf = [0u8; 256];
    let mut moved: u32 = 0;
    let mut off = dst_off;
    while moved < cap as u32 {
        let want = ((cap as u32 - moved).min(256)) as usize;
        let n = fd.stdio_drain(stream, &mut buf[..want]);
        if n == 0 {
            break;
        }
        unsafe {
            let dst = core::slice::from_raw_parts_mut(off as *mut u8, n);
            dst.copy_from_slice(&buf[..n]);
        }
        off = off.wrapping_add(n as u32);
        moved += n as u32;
    }
    moved
}

/// P2-006-007: host → kernel stdin feed. The driver writes up to 1 KiB of
/// terminal input at `STDIN_FEED_SCRATCH_OFF` (0x2100, doorbell reserved
/// region) and calls this export; the bytes land in the stdin device FIFO
/// and any process read on fd 0 drains them (EAGAIN when empty — the
/// bridge's async stdin retries). Bounded-drop policy mirrors the output
/// sinks; returns the number of bytes accepted.
#[no_mangle]
pub unsafe extern "C" fn nodine_stdin_feed(src_off: u32, len: u32) -> u32 {
    let Some(fd) = (unsafe { fdstate_mut() }) else {
        return 0;
    };
    if len == 0 || len > STDIN_FEED_SCRATCH_CAP {
        return 0;
    }
    // Only the documented scratch offset is accepted — a stray offset cannot
    // smuggle arbitrary kernel-memory reads into the FIFO.
    if src_off != STDIN_FEED_SCRATCH_OFF {
        return 0;
    }
    let bytes = unsafe { core::slice::from_raw_parts(src_off as *const u8, len as usize) };
    fd.stdin_feed(bytes);
    len
}

// ---------------------------------------------------------------------------
// no_std runtime bits
// ---------------------------------------------------------------------------

use core::panic::PanicInfo;

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    // The kernel must never panic; a panic is a kernel bug and must fail
    // loudly (trap) — never continue with corrupt state.
    core::arch::wasm32::unreachable()
}

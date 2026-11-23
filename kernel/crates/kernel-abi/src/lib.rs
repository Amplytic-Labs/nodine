//! kernel-abi — the normative Nodine kernel ABI definition.
//!
//! VERBATIM TRANSCRIPTION of KERNEL_ABI.md v1.0.1 §24 (frozen P2-002).
//! The markdown artifact is the source of truth; this file is generated from it
//! and MUST NOT drift (P2-003 establishes the sync check; any edit requires an
//! ABI minor-version bump and a matching §24 update).
//!
//! SPDX-License-Identifier: Apache-2.0 OR MIT (Nodine project)

#![allow(dead_code)]

//! kernel-abi — the Nodine kernel ABI, version 1.0.
//!
//! ONE ABI, two endpoint classes (MODE_SYNC / MODE_ASYNC). All layouts are
//! little-endian, 8-byte aligned, `#[repr(C)]`. Every constant here is part of
//! the frozen ABI v1.0: changing any of them is a MAJOR version bump (see
//! docs/phase2/KERNEL_ABI.md §20.3).
//!
//! Shared memory: one shared `WebAssembly.Memory` (the KSM) whose buffer is a
//! SharedArrayBuffer. All cross-agent words are accessed with SeqCst atomics
//! (JS `Atomics.*` are sequentially consistent by ECMA-262; Rust uses
//! `Ordering::SeqCst` to match — KERNEL_ABI.md §15).

#![no_std]

// ---------------------------------------------------------------------------
// 1. Magic, version
// ---------------------------------------------------------------------------

/// KSM magic at KCTL+0x00: ASCII "CARAPCE1", little-endian.
pub const ABI_MAGIC: u64 = 0x3145_4350_4152_4143;
/// Endpoint-table slot magic: ASCII "CARPES01", little-endian.
pub const ENDPOINT_SLOT_MAGIC: u64 = 0x3130_5345_5052_4143;
/// Endpoint-region header magic: ASCII "CARPEP01", little-endian.
pub const ENDPOINT_REGION_MAGIC: u64 = 0x3130_5045_5052_4143;

/// ABI major version. Layout/semantics changes bump this (hard incompatibility).
pub const ABI_VERSION_MAJOR: u16 = 1;
/// ABI minor version. Additive changes (new ops/caps within reserved ranges).
/// v1.0.1 (P2-002 freeze): adversarial amendments — producer ordering (§11.2),
/// completion-field ordering (§11.5/§15.6), sync TX cap 8128 B (§11.6/§17.1),
/// ingress op OP_INGRESS_READ (§7.6), persistence channel (§5.2/§13.5),
/// deferred-surface + v1.1 fused op-code reservations (§7.5). Minor bump only:
/// no layout change, no semantic change to existing op behavior.
/// v1.0.2 (P2-004, ADR-0010): diagnostic range 0x1000–0x10FF allocated +
/// OP_TRANSPORT_PROBE (§29/§7.7); §17.3 ASYNC_DEFER_LIMIT named = 40 passes;
/// §18 cancellation gap fills (cancel-own status 0 on both paths; cooperative
/// abort = ECANCELED kind NONE; pre-pass cancel resolution normative; sync-mode
/// OP_CANCEL ⇒ NODINE_EINVAL_REQUEST). Additive only: no layout change, no
/// semantic change to any existing op. `abi_compatible` keeps v1.0.1 bridges
/// working against a v1.0.2 kernel.
pub const ABI_VERSION_MINOR: u16 = 2;
/// Packed `abi_version` as stored in KCTL+0x08: `(major << 16) | minor`.
pub const ABI_VERSION: u32 = ((ABI_VERSION_MAJOR as u32) << 16) | (ABI_VERSION_MINOR as u32);

/// v1.0.1 normative sync payload cap (§11.6): TX_SYNC_RESERVE − 64.
pub const TX_SYNC_PAYLOAD_MAX: u32 = TX_SYNC_RESERVE - 64; // 8128
/// Phase-2 ingress op (§7.6) — exact-path module-serving read.
pub const OP_INGRESS_READ: u32 = 0x0140;
/// Reserved for v1.1 (§7.6); MUST execute the vendored resolve.js — never a
/// second resolver implementation.
pub const OP_INGRESS_RESOLVE: u32 = 0x0141;
/// v1.0-defined, P2-008 implementation (§7.5/§13.5): journal barrier
/// (drain + flush ack); persistence stall past the §11.3 ladder ⇒ EIO.
pub const OP_FSYNC: u32 = 0x0120;
/// v1.0.2 (ADR-0010 §7.7): transport-validation diagnostic op. Pure function
/// of args (a0 = result_len, a1 = fill, a2 = work_units), zero kernel-state
/// mutation, never mapped to a Node syscall. Enables the §9.3 RX-ledger
/// wrap test and §17.2/§18 behavior tests before the VFS exists (P2-005).
pub const OP_TRANSPORT_PROBE: u32 = 0x1000;
/// v1.0.2 (ADR-0010 §17.3 value fill): kernel-authoritative EAGAIN conversion
/// after this many consecutive deferred pump passes for one endpoint
/// (40 × ≤250 ms = 10 s = SYNC_TOTAL_TIMEOUT_MS; see KERNEL_ABI §29).
pub const ASYNC_DEFER_LIMIT: u32 = 40;
/// v1.0.2 §7.5: the diagnostic op range.
pub const OP_RANGE_DIAG_LO: u32 = 0x1000;
pub const OP_RANGE_DIAG_HI: u32 = 0x10FF;

/// Negotiation rule (KERNEL_ABI.md §20.2): kernel accepts a bridge iff
/// majors are equal and the kernel minor is >= the bridge minor.
pub const fn abi_compatible(kernel_version: u32, bridge_version: u32) -> bool {
    let (kmaj, kmin) = (kernel_version >> 16, kernel_version & 0xFFFF);
    let (bmaj, bmin) = (bridge_version >> 16, bridge_version & 0xFFFF);
    kmaj == bmaj && kmin >= bmin
}

// ---------------------------------------------------------------------------
// 2. KSM global map (absolute offsets — KERNEL_ABI.md §5.2)
// ---------------------------------------------------------------------------

pub const KCTL_OFFSET: u32 = 0x0000_0000;
pub const ENDPOINT_TABLE_OFFSET: u32 = 0x0000_1000;
pub const KERNEL_DOORBELL_OFFSET: u32 = 0x0000_2000;
pub const TRANSPORT_BASE: u32 = 0x0001_0000;
pub const KERNEL_HEAP_BASE: u32 = 0x0506_0000;

pub const MAX_ENDPOINTS: u32 = 64;
/// Endpoint region stride. 64 × EP_STRIDE = 0x0504_0000 (81 MiB, lazily committed).
pub const EP_STRIDE: u32 = 0x0014_1000;

/// `KSM_INITIAL_PAGES` = KERNEL_HEAP_BASE page + 1 MiB initial kernel heap.
pub const KSM_INITIAL_PAGES: u32 = 1302; // 0x516_0000 bytes ≈ 84.6 MiB
/// Default KSM maximum: 1 GiB (host-configurable before creation; wasm32 cap 4 GiB).
pub const KSM_DEFAULT_MAX_PAGES: u32 = 32768; // 2 GiB (was 1 GiB): VFS ceiling 1536 MiB for SDK-58

/// Absolute base of endpoint `i`'s region.
pub const fn endpoint_region_base(index: u32) -> u32 {
    TRANSPORT_BASE.wrapping_add(index.wrapping_mul(EP_STRIDE))
}
/// Absolute offset of endpoint `i`'s submit-ring header.
pub const fn submit_ring_header_off(index: u32) -> u32 {
    endpoint_region_base(index) + EP_SUBMIT_RING_HEADER_OFF
}
/// Absolute offset of the kernel doorbell word (the only global doorbell).
pub const KERNEL_DOORBELL_INDEX32: u32 = KERNEL_DOORBELL_OFFSET / 4;

// ---------------------------------------------------------------------------
// 3. Endpoint region sub-layout (endpoint-relative — KERNEL_ABI.md §5.5)
// ---------------------------------------------------------------------------

pub const EP_HEADER_OFF: u32 = 0x0000;
pub const EP_SUBMIT_RING_HEADER_OFF: u32 = 0x0040;
pub const EP_COMPLETION_RING_HEADER_OFF: u32 = 0x0050;
pub const EP_SYNC_MAILBOX_OFF: u32 = 0x0060;
pub const EP_TX_ARENA_OFF: u32 = 0x0100;
pub const EP_RX_ARENA_OFF: u32 = 0x4_0100;
pub const EP_SYNCRES_OFF: u32 = 0x8_0100;
pub const EP_SUBMIT_SLOTS_OFF: u32 = 0xC_0100;
pub const EP_COMPLETION_SLOTS_OFF: u32 = 0x10_0100;
// EP_STRIDE (0x14_1000) − EP_COMPLETION_SLOTS_OFF − RING_SLOTS*SLOT_SIZE = 0xF00 pad.

pub const TX_SIZE: u32 = 0x4_0000; // 256 KiB
pub const RX_SIZE: u32 = 0x4_0000; // 256 KiB
pub const SYNCRES_SIZE: u32 = 0x4_0000; // 256 KiB
pub const RING_SLOTS: u32 = 4096; // power of two (index mask below)
pub const RING_INDEX_MASK: u32 = RING_SLOTS - 1;
pub const SLOT_SIZE: u32 = 64;

/// Submit-ring slots reserved for MODE_SYNC requests (async must keep
/// `tail - head <= RING_SLOTS - SYNC_RING_SLOTS`). KERNEL_ABI.md §17.1.
pub const SYNC_RING_SLOTS: u32 = 64;
/// TX-arena tail reserve for sync payloads (paths). KERNEL_ABI.md §17.1.
pub const TX_SYNC_RESERVE: u32 = 0x2000; // 8 KiB
/// Max path bytes (excl. NUL). Longer ⇒ ENAMETOOLONG (uv -36).
pub const PATH_MAX: u32 = 4096;
/// Headroom the kernel keeps when checking result-area fit.
pub const RESULT_AREA_HEADROOM: u32 = 64;

// ---------------------------------------------------------------------------
// 4. Operation ids (KERNEL_ABI.md §7) — stable within an ABI major version
// ---------------------------------------------------------------------------

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    // control & capability (0x0001–0x000F)
    AbiProbe     = 0x0001,
    CapsQuery    = 0x0002,
    // process lifecycle (0x0010–0x001F)
    ProcReady    = 0x0010,
    ProcExit     = 0x0011,
    ProcSpawn    = 0x0012, // stub (Phase 3)
    ProcKill     = 0x0013, // stub (Phase 3)
    ProcWait     = 0x0014, // stub (Phase 3)
    // meta (0x00F0–0x00FF)
    Cancel       = 0x00F0,
    // filesystem v1 (0x0100–0x011F)
    Open         = 0x0100,
    Close        = 0x0101,
    Read         = 0x0102,
    Write        = 0x0103,
    Stat         = 0x0104,
    Lstat        = 0x0105,
    Fstat        = 0x0106,
    Mkdir        = 0x0107,
    Rmdir        = 0x0108,
    Unlink       = 0x0109,
    Rename       = 0x010A,
    Readdir      = 0x010B,
    Ftruncate    = 0x010C, // stub (P2-005 optional)
    Access       = 0x010D, // stub
    WatchStart   = 0x0110, // stub (P2-005+)
    WatchStop    = 0x0111, // stub (P2-005+)
    WatchEvent   = 0x0112, // reserved: unsolicited event completion op echo
    // diagnostic (0x1000–0x10FF, v1.0.2 ADR-0010 §7.7)
    TransportProbe = 0x1000,
}

impl Op {
    /// True if defined and implemented in v1.0 (stubs return NODINE_ENOOP).
    pub const fn implemented(self) -> bool {
        matches!(self,
            Op::AbiProbe | Op::CapsQuery
            | Op::ProcReady | Op::ProcExit
            | Op::Cancel
            | Op::Open | Op::Close | Op::Read | Op::Write
            | Op::Stat | Op::Lstat | Op::Fstat
            | Op::Mkdir | Op::Rmdir | Op::Unlink | Op::Rename | Op::Readdir
            | Op::TransportProbe)
    }
    /// True if the op takes a PATH (or PATH2) TX payload.
    pub const fn takes_path(self) -> bool {
        matches!(self, Op::Open | Op::Stat | Op::Lstat | Op::Mkdir | Op::Rmdir
            | Op::Unlink | Op::Rename | Op::Readdir | Op::Access
            | Op::WatchStart)
    }
}

// ---------------------------------------------------------------------------
// 5. Request flags, result kinds, completion flags (KERNEL_ABI.md §6/§8.4)
// ---------------------------------------------------------------------------

pub const FLAG_MODE_SYNC: u32 = 1 << 0;   // endpoint class 1 (§11); 0 = class 2 (§12)
pub const FLAG_STAT_BIGINT: u32 = 1 << 1; // STAT/LSTAT/FSTAT: 18 × i64 result
pub const FLAGS_KNOWN_MASK: u32 = FLAG_MODE_SYNC | FLAG_STAT_BIGINT;

pub const RESULT_NONE: u32 = 0;
pub const RESULT_INLINE: u32 = 1;
pub const RESULT_RX_ARENA: u32 = 2;   // legal on the completion ring only
pub const RESULT_SYNCRES: u32 = 3;    // legal in the sync mailbox only
pub const INLINE_CAP: u32 = 16;       // inline payload capacity of both records

pub const CFLAG_EVENT: u32 = 1 << 0;  // unsolicited event completion (watch stubs)

// ---------------------------------------------------------------------------
// 6. Lifecycle states (KERNEL_ABI.md §5.3/§5.4/§5.5/§11.5)
// ---------------------------------------------------------------------------

pub const KERNEL_STATE_BOOTING: u32 = 0;
pub const KERNEL_STATE_RUNNING: u32 = 1;
pub const KERNEL_STATE_DRAINING: u32 = 2;
pub const KERNEL_STATE_STOPPED: u32 = 3;

pub const ENDPOINT_STATE_FREE: u32 = 0;
pub const ENDPOINT_STATE_ALLOCATED: u32 = 1;
pub const ENDPOINT_STATE_RUNNING: u32 = 2;
pub const ENDPOINT_STATE_DRAINING: u32 = 3;
pub const ENDPOINT_STATE_DEAD: u32 = 4;

pub const ENDPOINT_CAP_SYNC: u32 = 1 << 0;
pub const ENDPOINT_CAP_ASYNC: u32 = 1 << 1;
pub const ENDPOINT_CAP_INGRESS: u32 = 1 << 2; // kernel-worker-side proxy endpoint (§13)

/// Sync-mailbox state machine (SeqCst CAS transitions — KERNEL_ABI.md §11.5).
pub const SM_STATE_IDLE: u32 = 0;
pub const SM_STATE_SUBMITTED: u32 = 1;
pub const SM_STATE_COMPLETED: u32 = 2;
pub const SM_STATE_ABANDONED: u32 = 3;

// Kernel capability bitmap (KCTL.kernel_caps — KERNEL_ABI.md §20.1)
pub const KERNEL_CAP_FS_V1: u32 = 1 << 0;
pub const KERNEL_CAP_WATCH: u32 = 1 << 1;
pub const KERNEL_CAP_PROC: u32 = 1 << 2;
pub const KERNEL_CAP_PERSIST: u32 = 1 << 3;

// ---------------------------------------------------------------------------
// 7. Error codes (KERNEL_ABI.md §14)
// ---------------------------------------------------------------------------

/// NODINE_* protocol/health errors: status values with no uv analog.
/// uv errnos (Appendix A of KERNEL_ABI.md; values frozen from Node v26.13.0
/// Linux) occupy -1..-4095 and are re-exported in `uv` below.
pub const NODINE_EINVAL_REQUEST: i32 = -0x1000_0000;
pub const NODINE_ENOOP: i32 = -0x1000_0001;
pub const NODINE_EABI_VERSION: i32 = -0x1000_0002;
pub const NODINE_ESHUTDOWN: i32 = -0x1000_0003;
pub const NODINE_ETIMEOUT: i32 = -0x1000_0004;
pub const NODINE_EKERNEL_DEAD: i32 = -0x1000_0005;
pub const NODINE_EENDPOINT: i32 = -0x1000_0006;
pub const NODINE_EPROTOCOL: i32 = -0x1000_0007;
pub const NODINE_EBADCORRELATION: i32 = -0x1000_0008;

/// The libuv errno table (Linux), byte-identical to
/// poc/p1-poc/uv-errno-linux.txt (generated from Node v26.13.0's vendored
/// deps/uv headers + system errno.h). Status values -1..-4095.
pub mod uv {
    macro_rules! uv_errno {
        ($($name:ident = $val:expr),* $(,)?) => {
            $(pub const $name: i32 = $val;)*
        };
    }
    uv_errno! {
        E2BIG = -7, EACCES = -13, EADDRINUSE = -98, EADDRNOTAVAIL = -99,
        EAFNOSUPPORT = -97, EAGAIN = -11, EAI_ADDRFAMILY = -3000,
        EAI_AGAIN = -3001, EAI_BADFLAGS = -3002, EAI_BADHINTS = -3013,
        EAI_CANCELED = -3003, EAI_FAIL = -3004, EAI_FAMILY = -3005,
        EAI_MEMORY = -3006, EAI_NODATA = -3007, EAI_NONAME = -3008,
        EAI_OVERFLOW = -3009, EAI_PROTOCOL = -3014, EAI_SERVICE = -3010,
        EAI_SOCKTYPE = -3011, EALREADY = -114, EBADF = -9, EBUSY = -16,
        ECANCELED = -125, ECHARSET = -4080, ECONNABORTED = -103,
        ECONNREFUSED = -111, ECONNRESET = -104, EDESTADDRREQ = -89,
        EEXIST = -17, EFAULT = -14, EFBIG = -27, EHOSTUNREACH = -113,
        EINTR = -4, EINVAL = -22, EIO = -5, EISCONN = -106, EISDIR = -21,
        ELOOP = -40, EMFILE = -24, EMSGSIZE = -90, ENAMETOOLONG = -36,
        ENETDOWN = -100, ENETUNREACH = -101, ENFILE = -23, ENOBUFS = -105,
        ENODEV = -19, ENOENT = -2, ENOMEM = -12, ENONET = -64,
        ENOPROTOOPT = -92, ENOSPC = -28, ENOSYS = -38, ENOTCONN = -107,
        ENOTDIR = -20, ENOTEMPTY = -39, ENOTSOCK = -88, ENOTSUP = -95,
        EOVERFLOW = -75, EPERM = -1, EPIPE = -32, EPROTO = -71,
        EPROTONOSUPPORT = -93, EPROTOTYPE = -91, ERANGE = -34, EROFS = -30,
        ESHUTDOWN = -108, ESPIPE = -29, ESRCH = -3, ETIMEDOUT = -110,
        ETXTBSY = -26, EXDEV = -18, UNKNOWN = -4094,
    }
}

// ---------------------------------------------------------------------------
// 8. Wire structures (KERNEL_ABI.md §5.3, §5.4, §5.5, §6)
// ---------------------------------------------------------------------------

/// KCTL: kernel control block at KSM offset 0 (64 bytes used, 4 KiB reserved).
/// Write discipline (§15.4): kernel SeqCst-stores all once-fields, then
/// `kernel_state = RUNNING`; readers load `kernel_state` first.
#[repr(C)]
pub struct ControlBlock {
    pub magic: u64,               // +0x00 ABI_MAGIC
    pub abi_version: u32,         // +0x08 ABI_VERSION
    pub generation: u32,          // +0x0C boot generation
    pub kernel_state: u32,        // +0x10 KERNEL_STATE_*
    pub kernel_caps: u32,         // +0x14 KERNEL_CAP_* bitmap
    pub max_endpoints: u32,       // +0x18 MAX_ENDPOINTS
    pub endpoint_stride: u32,     // +0x1C EP_STRIDE
    pub transport_base: u32,      // +0x20 TRANSPORT_BASE
    pub kernel_heap_base: u32,    // +0x24 KERNEL_HEAP_BASE
    pub kernel_heartbeat_ms: u64, // +0x28 Date.now() at last pump pass
    pub doorbell_offset: u32,     // +0x30 KERNEL_DOORBELL_OFFSET
    pub ksm_pages_current: u32,   // +0x34
    pub ksm_pages_max: u32,       // +0x38
    pub reserved0: u32,           // +0x3C
}

/// Endpoint-table slot at ENDPOINT_TABLE_OFFSET + i*64 (§5.4).
#[repr(C)]
pub struct EndpointSlot {
    pub magic: u64,        // +0x00 ENDPOINT_SLOT_MAGIC
    pub ep_base: u32,      // +0x08 absolute KSM offset of the endpoint region
    pub pid: u32,          // +0x0C owning process id
    pub generation: u32,   // +0x10
    pub state: u32,        // +0x14 ENDPOINT_STATE_*
    pub ep_caps: u32,      // +0x18 ENDPOINT_CAP_*
    pub reserved0: u32,    // +0x1C
    pub owner_tag: u64,    // +0x20 diagnostic: host worker-instance id
    pub reserved1: u64,    // +0x28
    pub reserved2: u64,    // +0x30
    pub reserved3: u64,    // +0x38
}

/// Live endpoint header at EP_HEADER_OFF (§5.5).
#[repr(C)]
pub struct EndpointHeader {
    pub ep_magic: u64,          // +0x00 ENDPOINT_REGION_MAGIC
    pub ep_pid: u32,            // +0x08
    pub ep_generation: u32,     // +0x0C
    pub ep_state: u32,          // +0x10 ENDPOINT_STATE_*
    pub ep_flags: u32,          // +0x14 reserved (0)
    pub ep_heartbeat_ms: u64,   // +0x18 bridge-written liveness (§21.4)
    pub reserved: [u8; 32],     // +0x20
}

/// Ring header pair (submit ring at +0x40, completion ring at +0x50; §5.5).
/// `sr.head`/`cr.tail` writer rules and orderings: §15.4.
#[repr(C)]
pub struct RingHeader {
    pub head: u32,   // consumer-owned index (mod 2^32; slot = idx & RING_INDEX_MASK)
    pub tail: u32,   // producer-owned index
    pub extra: u32,  // submit ring: sr_sync_pending | completion ring: cr_seq doorbell
    pub reserved: u32,
}

/// RequestRecord: 64 B, one per submit-ring slot (§6.1). Producer-written only.
/// `tx_off`/`tx_len` reference the producer's TX arena; `a0..a4` per op table §7.
#[repr(C)]
pub struct RequestRecord {
    pub req_id: u64, // +0x00 per-endpoint monotonic (§8.1)
    pub op: u32,     // +0x08 Op as u32
    pub flags: u32,  // +0x0C FLAG_* (unknown bits must be 0)
    pub tx_off: u32, // +0x10 TX-arena offset, 8-aligned
    pub tx_len: u32, // +0x14 TX payload length (0 = none)
    pub a0: u64,     // +0x18
    pub a1: u64,     // +0x20
    pub a2: u64,     // +0x28
    pub a3: u64,     // +0x30
    pub a4: u64,     // +0x38
}

/// CompletionRecord: 64 B, one per completion-ring slot (§6.2). Kernel-written
/// only. The sync mailbox embeds the same completion fields (§6.3) — one schema.
#[repr(C)]
pub struct CompletionRecord {
    pub req_id: u64,     // +0x00 completed request id (events: watch id)
    pub op: u32,         // +0x08 op echo
    pub status: i32,     // +0x0C 0 | uv errno | NODINE_* (§14)
    pub result_kind: u32,// +0x10 RESULT_*
    pub result_len: u32, // +0x14
    pub result_off: u32, // +0x18 area-relative, 8-aligned
    pub cflags: u32,     // +0x1C CFLAG_*
    pub inline_: [u8; 16], // +0x20 RESULT_INLINE payload
    pub reserved: [u8; 16],// +0x30
}

/// Sync mailbox at EP_SYNC_MAILBOX_OFF (§6.3): completion cell + doorbell +
/// abandon state machine (§11.5). `sm_seq` is the sync wait doorbell.
#[repr(C)]
pub struct SyncMailbox {
    pub sm_seq: u32,     // +0x00 kernel-incremented completion counter (doorbell)
    pub sm_state: u32,   // +0x04 SM_STATE_* (SeqCst CAS both sides)
    pub sm_op: u32,      // +0x08 op echo
    pub reserved0: u32,  // +0x0C
    pub sm_req_id: u64,  // +0x10
    pub sm_status: i32,  // +0x18
    pub sm_kind: u32,    // +0x1C RESULT_NONE | RESULT_INLINE | RESULT_SYNCRES (never RX)
    pub sm_off: u32,     // +0x20 sync-result-area offset
    pub sm_len: u32,     // +0x24
    pub sm_inline: [u8; 16], // +0x28
    pub reserved1: u64,  // +0x38
}

// ---------------------------------------------------------------------------
// 9. Size/alignment assertions — merge blockers (directive §3 GATE C)
// ---------------------------------------------------------------------------

macro_rules! static_assert_size {
    ($ty:ty, $size:expr) => {
        const _: () = {
            let actual = core::mem::size_of::<$ty>();
            assert!(actual == $size,
                concat!(stringify!($ty), " must be exactly ", stringify!($size),
                        " bytes (was ", stringify!($size), " expected); ABI layout drift"));
            let _ = actual;
        };
    };
}
macro_rules! static_assert_align {
    ($ty:ty, $align:expr) => {
        const _: () = {
            let actual = core::mem::align_of::<$ty>();
            assert!(actual == $align, concat!(stringify!($ty), " alignment drift"));
            let _ = actual;
        };
    };
}

static_assert_size!(ControlBlock, 64);
static_assert_align!(ControlBlock, 8);
static_assert_size!(EndpointSlot, 64);
static_assert_align!(EndpointSlot, 8);
static_assert_size!(EndpointHeader, 64);
static_assert_align!(EndpointHeader, 8);
static_assert_size!(RingHeader, 16);
static_assert_align!(RingHeader, 4);
static_assert_size!(RequestRecord, 64);
static_assert_align!(RequestRecord, 8);
static_assert_size!(CompletionRecord, 64);
static_assert_align!(CompletionRecord, 8);
static_assert_size!(SyncMailbox, 64);
static_assert_align!(SyncMailbox, 8);

const _: () = assert!(RING_SLOTS.is_power_of_two(), "RING_SLOTS must be a power of two");
const _: () = assert!(EP_STRIDE % 8 == 0 && TX_SIZE % 8 == 0 && RX_SIZE % 8 == 0
    && SYNCRES_SIZE % 8 == 0, "all regions 8-byte aligned");
const _: () = assert!(
    (ENDPOINT_TABLE_OFFSET as u64) + (MAX_ENDPOINTS as u64) * 64 <= TRANSPORT_BASE as u64,
    "endpoint table must not overlap the transport pool"
);
const _: () = assert!(
    (TRANSPORT_BASE as u64) + (MAX_ENDPOINTS as u64) * (EP_STRIDE as u64)
        <= KERNEL_HEAP_BASE as u64,
    "transport pool must not overlap the kernel heap"
);
const _: () = assert!(
    (KERNEL_HEAP_BASE as u64) + 0x10_0000 == (KSM_INITIAL_PAGES as u64) * 65_536,
    "KSM_INITIAL_PAGES must cover the fixed map + 1 MiB initial kernel heap"
);
const _: () = assert!(INLINE_CAP == 16 && core::mem::size_of::<CompletionRecord>() == 64);

// ---------------------------------------------------------------------------
// 10. Doorbell / ring helper documentation (behavioral contract, not code)
// ---------------------------------------------------------------------------
//
// Kernel doorbell (global, u32 @ KERNEL_DOORBELL_OFFSET):
//   producer : Atomics.add(doorbell, 1); Atomics.notify(doorbell, 1);
//   kernel   : epoch = SeqCstLoad(doorbell); pump(); if idle
//              Atomics.wait(doorbell, epoch, timeout<=250ms);
//   JS-side indices: Int32Array over ksm.buffer, index KERNEL_DOORBELL_INDEX32.
//
// Completion doorbell (per endpoint, cr_seq = RingHeader.extra of the
// completion ring): kernel increments (SeqCst store) after each cr_tail store,
// then notify(1); bridge arms Atomics.waitAsync(cr_seq, expected) with
// expected loaded BEFORE draining (no-lost-wakeup: KERNEL_ABI.md §15.5).
//
// Sync doorbell (per endpoint, sm_seq): kernel increments + notify on sync
// completion / abandon-reset; blocked worker waits with the §11.3 rung ladder.
//
// Ring arithmetic (per ring): slot(i) = slots_base + (i & RING_INDEX_MASK)*64;
// used = tail.wrapping_sub(head); full-sync never (reserves); full-async
// (submit ring) = used > RING_SLOTS - SYNC_RING_SLOTS; empty = used == 0.

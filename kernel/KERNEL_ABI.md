# Kernel ABI Specification — ONE ABI, Two Endpoint Classes (GATE C)

Task ID: **P2-SA-B** · Date: 2026-11-09 · Status: **v1.0.2 — v1.0.1 body preserved verbatim; §29 delta appended per ADR-0010 (P2-004-002 audit); two documentation errata applied at P2-005-003 (§6.4 sync bound −8 → −64, matching §7.3/§11.6 and the shipped transport; §7.3 OP_READDIR cookie parenthetical "entry index" → "dentry sequence number") — wire behavior unchanged** ·
Artifact class: **Phase 2 architecture specification — FROZEN (P2-002), minor-bumped v1.0.2 (P2-004)**

> **v1.0.1 changelog (P2-002 freeze):** §11.2 producer sequence reordered (mailbox
> CAS precedes ring publish — F-B2); §11.5 complete-CAS residual-state handling
> completed + pre-publish error rule; §11.5/§15.6 completion-field ordering
> corrected to fields-BEFORE-CAS (F-B1, demonstrated by torture T2); §11.6/§17.1
> sync TX payload cap = `TX_SYNC_RESERVE − 64` = 8128 B with mandatory bridge
> chunking (F-B3/T4); §11.3 per-op execution budget bound (F-B4/T3); §9.3 RX
> per-record byte ledger (F-B6); §8.1 outstanding-table carve-out for EVENT
> completions (F-B10); §7.5/§7.6 `OP_INGRESS_READ` + deferred-surface and
> v1.1 fused op-code reservations + `OP_FSYNC` decision (G2b-C1, deltas 1/4);
> §5.2/§13.5 persistence-channel memory home, F-owned layout (G2b-C5); §8.3
> fd caps → EMFILE/ENFILE + stdio device OFDs (delta 5, ADR-0008); §23 fsync
> anchor scope note (G2b-C4); §14.3 citations re-pointed at the bug-fixed
> gate-probe evidence (C12). Verified against the P2-SA-G2a torture suite
> (T1 PASS after mock fixes; T2 shows the old text's failure mode; T4
> arithmetic resolved).

This document specifies the single binary kernel ABI of Nodine: the shared-memory
interface between the Node.js runtime workers (via the binding bridge) and the
Rust/WASM kernel worker. It is written to be implementable by a third engineer
without further questions. The Rust ABI-definition artifact in §24 is the normative
source of constants and layouts; all tables in this document agree with it exactly.

Evidence labels (project convention): **VERIFIED** (cite file:line, spec text, or
experiment with command+output) · **INFERRED** · **EXPERIMENTAL** (measured,
run-to-run noise possible) · **UNKNOWN** · **BLOCKED**. All numeric measurements
come from the Phase 1 experiments under `poc/p1-c/results/` (canonical run
2026-11-08, Chromium 143.0.7499.4, 2-CPU box, cross-origin-isolated; reproducibility
re-run identical on 18/18 verdicts — `docs/phase1/BROWSER_EXECUTION.md`).

---

## Table of contents

1. [Overview, goals, non-goals](#1-overview-goals-non-goals)
2. [Terminology and agent inventory](#2-terminology-and-agent-inventory)
3. [The two endpoint classes (one interface, two execution modes)](#3-the-two-endpoint-classes)
4. [Design inputs — measured and documented evidence](#4-design-inputs)
5. [Shared-memory map (KSM)](#5-shared-memory-map)
6. [Core data structures (byte-level)](#6-core-data-structures)
7. [Operation table](#7-operation-table)
8. [Identifiers, flags, and versioning](#8-identifiers-flags-and-versioning)
9. [Data arenas, ownership, and buffer lifetime](#9-data-arenas-ownership-and-buffer-lifetime)
10. [Ring mechanics](#10-ring-mechanics)
11. [Endpoint class 1 — synchronous request/reply protocol](#11-endpoint-class-1--synchronous-requestreply-protocol)
12. [Endpoint class 2 — asynchronous request/completion protocol](#12-endpoint-class-2--asynchronous-requestcompletion-protocol)
13. [Service Worker async path (bridged, not a second ABI)](#13-service-worker-async-path)
14. [Error model and Node error mapping](#14-error-model-and-node-error-mapping)
15. [Memory ordering and happens-before proofs](#15-memory-ordering-and-happens-before-proofs)
16. [Deadlock-freedom analysis](#16-deadlock-freedom-analysis)
17. [Backpressure and queue-full policies](#17-backpressure-and-queue-full-policies)
18. [Cancellation](#18-cancellation)
19. [Invalid request handling, validation, and trust model](#19-invalid-request-handling-validation-and-trust-model)
20. [ABI version negotiation and compatibility rules](#20-abi-version-negotiation-and-compatibility-rules)
21. [Shutdown, kernel death, and worker death](#21-shutdown-kernel-death-and-worker-death)
22. [Single-kernel-worker consequences (honest limitations)](#22-single-kernel-worker-consequences)
23. [Performance budget](#23-performance-budget)
24. [Rust ABI-definition artifact (normative)](#24-rust-abi-definition-artifact-normative)
25. [Directive §3 GATE C completeness checklist](#25-directive-3-gate-c-completeness-checklist)
26. [Open questions for the main agent](#26-open-questions-for-the-main-agent)
27. [Appendix A — libuv errno table (Linux, Node v26.13.0)](#27-appendix-a--libuv-errno-table)
28. [Appendix B — worked byte-level examples](#28-appendix-b--worked-byte-level-examples)

---

## 1. Overview, goals, non-goals

Nodine executes upstream, unmodified Node.js v26.13.0 inside Chromium Dedicated
Workers (`docs/REPORT_PHASE1.md` §1). Phase 2 replaces the Phase 1 JavaScript
in-worker VFS seam (`poc/p1-poc/worker.mjs` — the 39-binding PoC) with a real
Rust kernel compiled to `wasm32-unknown-unknown`, running inside its own Dedicated
Worker (ADR-0002). The kernel owns the VFS, file descriptors, the process table,
and (later) scheduling, virtual networking, and the CAS store (ADR-0003,
`docs/ARCHITECTURE.md` §2–§4).

**This document defines the one and only interface across that boundary.** It is a
binary ABI over one shared memory, with exactly two endpoint classes:

- **Class 1 — synchronous request/reply:** the runtime worker submits a request and
  must receive the actual result before returning to upstream Node code
  (`statSync`, `readFileSync`, `openSync`, …). The runtime worker blocks in
  `Atomics.wait` on its own completion mailbox (legal and measured — P1-C2 E5-a);
  the kernel worker continues to execute independently.
- **Class 2 — asynchronous request/completion:** the same request format is
  submitted through the same rings; the caller receives a request id and keeps its
  event loop alive; completions are delivered through a per-process completion ring
  plus a wake. The Service Worker participates in this class exclusively through a
  kernel-worker-side MessageChannel bridge that uses the identical record formats
  (§13).

**Hard properties (violating any is a spec bug):**

1. ONE ABI. Both classes share operation definitions, versioning, error codes,
   memory representation, identifier namespaces, fs semantics, request metadata,
   and the completion schema. They differ only in *mode flag*, *completion
   delivery lane*, and *wake mechanism*.
2. No JSON, no structured cloning, no `postMessage` on any hot path. Binary
   fixed-size records + offset/length-referenced payload bytes in shared memory.
   `postMessage`/`MessageChannel` are used only for worker bootstrap, the host
   control plane, and the SW bridge (§13).
3. No busy-wait polling anywhere (P1-C2 E5 phase d measured spin at p50 0 ms but
   it monopolizes a CPU — rejected as policy; phase a, both sides block in
   `Atomics.wait`, is the canonical discipline).
4. No fallback paths. Fixed capacities, explicit errors, fail-fast on missing
   capabilities (`ERR_NODINE_UNSUPPORTED_ENVIRONMENT` — 14-check gate, P1-C2 §5).
5. Mismatched ABI version, unknown op id, out-of-range offsets, wrong-direction
   access → explicit error completion, never UB, never silent corruption.
6. The main thread and the Service Worker never call `Atomics.wait`
   (VERIFIED: `Atomics.wait` throws outside workers — `poc/p1-c/results/e2-sab-atomics.json`
   → `result.mainThreadWaits`; SW per P1-F).

**Non-goals (Phase 2):** networking/vTCP ops (Phase 5 — the SW ingress *mechanism*
is nevertheless specified here, §13, because it constrains the ABI), `worker_threads`
multi-endpoint usage (the layout supports it, §5.4, but only one endpoint per
process is exercised in Phase 2), zero-copy/lent buffers (§9.6 documents the
two-copy design and the versioned path to fewer copies), JSPI (optional future
accelerator, never a dependency — ARCHITECTURE §4).

**What the ABI serves:** the binding bridge in each runtime worker re-exposes the
Node internalBinding surface (`fs`, `fs_dir`, …) exactly as the Phase 1 PoC did
(`poc/p1-poc/worker.mjs:1510-1866` — the bridge contract seed); upstream Node
`lib/*.js` then behaves unmodified. Subagent E's
`docs/phase2/NODE_FS_KERNEL_INTEGRATION.md` derives the full bridge contract; this
document defines the kernel side beneath it.

## 2. Terminology and agent inventory

| Term | Definition |
|---|---|
| **KSM** | Kernel Shared Memory: one shared `WebAssembly.Memory` (`{shared:true}`) whose `.buffer` is a `SharedArrayBuffer`. Created once by the kernel worker at boot. Contains the ABI control block, endpoint table, doorbells, per-process transport regions, and the kernel heap. |
| **Kernel worker** | The single Dedicated Worker that instantiates `nodine-kernel.wasm` (Rust, `wasm32-unknown-unknown`) over the KSM and runs the kernel event loop. |
| **Runtime worker / process worker** | A Dedicated Worker hosting exactly one Node.js runtime instance = one kernel process (ADR-0003). |
| **Bridge** | The Rust/WASM binding-bridge module running inside a runtime worker; it translates Node internalBinding calls into ABI requests. |
| **Process** | Kernel state: pid, argv, env, cwd, fd table, exit code, parentage (ADR-0003). |
| **Endpoint** | The per-submitter transport region in the KSM: submit ring, completion ring, sync mailbox, TX arena, RX arena, sync-result area. Phase 2: exactly one endpoint per process (the process's runtime worker). The endpoint abstraction exists so `worker_threads` (Phase 3) can get additional endpoints without any ABI change. |
| **Producer** | The single agent that writes a given submit ring (the endpoint's worker thread). |
| **Consumer** | The agent that dequeues: the kernel worker for submit rings; the endpoint's worker for completion rings. |
| **Doorbell** | A u32 epoch/counter word that a waiter blocks on (`Atomics.wait`/`waitAsync`) and a poster bumps (`Atomics.add` + `Atomics.notify`). |
| **TX arena** | Producer-owned payload area for request bytes (paths, write data). |
| **RX arena** | Kernel-owned payload area for async completion bytes (read data, stat arrays, readdir batches). |
| **Sync-result area** | Kernel-written result area dedicated to the (single) outstanding sync request of an endpoint. |
| **Sync mailbox** | 64-byte fixed cell per endpoint through which a sync request's completion is delivered and on which the runtime worker waits. |
| **OFD / kfd** | Open File Description, kernel-global, u64 id, never reused. Analogous to a POSIX open file description. |
| **fd** | Per-process file-descriptor number (small int, POSIX-shaped; 0/1/2 = stdio), mapped by the kernel to an OFD. |
| **Host** | The main-thread TypeScript boot orchestrator. Owns Worker creation, SW registration, and the control plane. No kernel logic. |

Agent inventory and their KSM access rights:

| Agent | Atomics.wait? | KSM access | Role |
|---|---|---|---|
| Main thread (host) | never (throws — E2 VERIFIED) | read KCTL/endpoint table only | boot, worker lifecycle, control plane |
| Kernel worker | yes (its doorbell, timeout-laddered) | full | the kernel |
| Runtime worker (bridge) | yes (sync mailbox; `waitAsync` for async) | its endpoint region + KCTL/doorbells (read) | Node process |
| Persistence worker (P2-008) | yes (may act as an endpoint) | its endpoint region | OPFS journal/snapshot |
| Service Worker | **never** (P1-F; `wait` throws in non-blockable agents) | **none** | preview ingress; talks to the kernel only via MessageChannel (§13) |
| WASI workers | as endpoints (later phases) | their endpoint regions | `@pnpm/wasm` etc. |

## 3. The two endpoint classes

One interface, two execution modes. Every request is a `RequestRecord` (§6.1) on
the endpoint's submit ring; bit 0 of `flags` selects the mode. Everything else —
op ids, argument encoding, error codes, arena rules, identifier namespaces, result
schema — is identical.

```
                 ONE ABI  (op table · records · arenas · errors · ids · version)
   ┌────────────────────────────────┴────────────────────────────────┐
   │ MODE_SYNC (class 1)                     MODE_ASYNC (class 2)    │
   │ request → submit ring (priority lane)   request → submit ring   │
   │ worker: Atomics.wait(sync mailbox)      caller gets req_id now  │
   │ kernel: executes, writes completion     kernel: executes,       │
   │        into the sync mailbox, notifies     writes CompletionRecord│
   │ worker: wakes with the ACTUAL result       into the completion   │
   │        (result available before return)    ring, notifies;      │
   │                                        worker: waitAsync wake → │
   │                                        retire → JS callback /   │
   │                                        promise resolution       │
   │ SW (async only): kernel-worker-side proxy posts the SAME record │
   │ formats over a MessageChannel (§13) — never a second ABI.       │
   └─────────────────────────────────────────────────────────────────┘
```

Why the two delivery lanes exist (and why this is still one ABI):

- A blocked runtime worker cannot retire completion-ring entries, so a sync
  completion must never depend on (a) completion-ring space, (b) RX-arena space,
  or (c) any consumer action. Hence the dedicated sync mailbox + sync-result area
  (§11, deadlock proof §16).
- An async caller must never block; its completions arrive on the completion ring
  and the wake is `Atomics.waitAsync` (semantics verified — E5-b: event loop keeps
  running while a `waitAsync` is pending, `timerFiredWhileWaiting: true`).
- The **record formats are byte-identical** between the lanes: the sync mailbox
  embeds the same completion fields (`req_id`, `op` echo, `status`, result kind /
  off / len / inline) as the `CompletionRecord`. A completion consumer parses one
  schema. The lanes differ only in *where the record sits* and *how the consumer is
  woken* — transport, not interface.

## 4. Design inputs

Measured prior art (`poc/p1-c/`, canonical results; all [EXPERIMENTAL] with
reproducibility re-run, 1000 iterations unless noted):

| # | Fact | Value | Source |
|---|---|---|---|
| M1 | Worker→worker SAB round trip, both sides `Atomics.wait`/`notify` | **p50 0.01 ms, p95 0.015, p99 0.025, max 0.46** | `results/e5-kernel-call.json → phases.a_wait_wait.stats` |
| M2 | Same, caller `waitAsync` | p50 0.015, p95 0.025, max 0.33 | `phases.b_waitAsync_wait.stats` |
| M3 | Kernel busy-spin (rejected policy) | p50 0 ms but monopolizes CPU; 2-CPU box | `phases.d_wait_spin.stats`; BROWSER_EXECUTION §4 Q2 |
| M4 | Kernel 1 ms-timeout wait (rejected: 24.59 ms tail) | p50 0.01, max 24.59 | `phases.e_wait_timeout1ms.stats` |
| M5 | MessageChannel round trip | p50 0.03, p95 0.04, **max 22.135** | `phases.c_messagechannel.stats` |
| M6 | `Atomics.wait` on main thread throws `TypeError` | infinite + timed | `results/e2-sab-atomics.json → result.mainThreadWaits` |
| M7 | wasm atomics on shared linear memory (cross-agent, both agents in wasm) | p50 0.005 ms | `results/e3-wasm-threads-isolated.json → pingPong` |
| M8 | OPFS sync-access-handle 256 B write+flush+read cycle | p50 0.785 ms | `results/e4-opfs.json → result.worker.cycle` |
| M9 | `waitAsync` keeps the event loop alive | `semanticsOk: true` | `phases.b_waitAsync_wait.waitAsyncSemantics` |
| M10 | Shared wasm memory grows in place, views stay valid | `oldBufferDetached:false, dataPreservedAcrossGrow:true` | `results/e6-memory-ceiling.json → variants.shared.grow` |
| M11 | Untouched wasm pages cost ~KiB RSS (lazy commit) | 211 KiB per untouched 16 MiB shared instance | `variants.shared.trial.committedPerInstanceKB` |
| M12 | Per-context wasm reservation ceiling ≈ 1.92 GiB | 123 × 16 MiB | `variants.shared.main` |
| M13 | `terminate()` p50 0.01 ms; worker spawn→first-pong p50 7.915 ms; shared buffers survive worker death | — | `results/e7-lifecycle.json` |

Design-relevant conclusions (with BROWSER_EXECUTION.md §4 as the derivation):

- **T1. Transport = SAB rings + Atomics, both directions, hot path only.** 3×
  better p50 and 48× better tail than MessageChannel (M1 vs M5); the tail is
  decisive for sync syscalls. [VERIFIED comparison]
- **T2. Every process is a Dedicated Worker** (M6 + ADR-0003): each process worker
  may block in `Atomics.wait`, blocking only itself.
- **T3. The kernel worker blocks on its doorbell** (M1 phase a). Its JS driver
  never blocks inside wasm code (§10.5), so the SW bridge and control plane always
  regain the kernel worker's thread.
- **T4. Async wake = `Atomics.waitAsync`** in runtime workers (M9); the SW gets
  completions via MessageChannel from the kernel worker (P1-F, §13).
- **T5. Result payloads move by pointer into shared memory, never cloned** (T1;
  ARCHITECTURE §4). The Phase 2 design uses exactly one kernel-side copy
  VFS→result area and one bridge-side copy result area→Node object (§9.6).
- **T6. Fixed-size arenas with lazy commit** (M11): reserving generous fixed
  regions costs reserved address space, not RSS.
- **T7. `poc/p1-poc/worker.mjs` is the bridge contract seed**: its `fs` binding
  (`open/close/read/writeBuffer/stat/fstat/mkdir/ReadFileJob/WriteFileJob/
  FSReqCallback`, Float64Array(18) stats, uv-errno error objects) is the exact
  surface the bridge re-exposes; its async completions arrived FIFO via the
  platform event loop and the Phase 1 event-ordering matrix was byte-identical to
  real Node (REPORT_PHASE1 §1) — evidence that per-process FIFO async completion
  is Node-conformant (libuv imposes no cross-op ordering).
- **T8. uv errno backbone**: `poc/p1-poc/uv-errno-linux.txt` (Appendix A) is the
  errno table the kernel returns; the PoC's `uvThrow`/`UVMAP` mapping
  (`worker.mjs:1519-1523, 1851-1864, 1486-1508`) is the Node error-shape template
  (§14).

---

## 5. Shared-memory map

### 5.1 The KSM and its creation

The **KSM** (kernel shared memory) is a single `WebAssembly.Memory` created by the
kernel worker at boot:

```js
// kernel worker bootstrap (control plane — the ONLY producer of the KSM)
const ksm = new WebAssembly.Memory({
  initial: 1302,          // KSM_INITIAL_PAGES (§5.2) — covers the fixed map + 1 MiB heap
  maximum: 16384,         // KSM_DEFAULT_MAX_PAGES = 1 GiB; host-configurable pre-creation
  shared: true,           // ⇒ ksm.buffer is a SharedArrayBuffer [VERIFIED: M10]
});
```

`ksm.buffer` is a `SharedArrayBuffer`; every runtime worker receives it (once, via
`postMessage` at worker bootstrap — control plane) and builds typed-array views
into its own endpoint region. The kernel module is instantiated over this same
memory; the kernel heap (Rust allocator domain) starts at `kernel_heap_base`
(§5.6) and grows via `memory.grow` — in place, without detaching views (M10,
VERIFIED). All agents that hold the KSM are inside one agent cluster
(ECMAScript SharedArrayBuffer memory model).

**Endianness:** little-endian everywhere (WebAssembly linear memory is
little-endian by specification; all engines implementing shared wasm memory are
LE). A boot-time self-check writes `u64 0x0102030405060708` into a scratch word
and verifies the byte order; mismatch aborts with
`ERR_NODINE_UNSUPPORTED_ENVIRONMENT` (explicit failure, never silent).

**Alignment:** every multi-byte field, record, and sub-region is 8-byte aligned.
Rationale: (a) WebAssembly atomic instructions trap on non-naturally-aligned
accesses (WebAssembly threads extension) — every doorbell/index word must be
naturally aligned; (b) JS typed-array views (`Float64Array`, `BigInt64Array`)
require byte offsets divisible by the element size or the constructor throws
`RangeError` — stat results must be viewable in place; (c) `#[repr(C)]` Rust
structs require natural alignment for defined behavior. 64-byte record size
additionally matches a cache line, keeping hot records non-split.

### 5.2 Global map (absolute KSM offsets — ABI v1.0 constants)

| Region | Range (hex) | Size | Owner | Contents |
|---|---|---|---|---|
| **KCTL** — kernel control block | `0x0000_0000 – 0x0000_0FFF` | 4 KiB (64 B used) | kernel writes, all read | §5.3 |
| **Endpoint table** | `0x0000_1000 – 0x0000_1FFF` | 64 × 64 B | kernel writes, all read | §5.4 |
| **Kernel doorbell** | `0x0000_2000 – 0x0000_FFFF` | u32 at `0x2000`, rest reserved | producers bump; kernel waits | §10.4 |
| **Transport pool** | `0x0001_0000 – 0x0504_FFFF` | 64 × `EP_STRIDE` | per-endpoint | §5.5 |
| **Persistence channel** (v1.0.1, §13.5) | `0x0505_0000 – 0x0505_FFFF` | 64 KiB | kernel + persistence worker | layout: OPFS_PERSISTENCE.md §4 (F-owned); P2-008 |
| **Kernel heap** | `0x0506_0000 – …` | grows to `maximum` | kernel only | Rust allocator domain |

Derived constants: `MAX_ENDPOINTS = 64`; `EP_STRIDE = 0x0014_1000` (1,314,816 B);
transport pool size = `64 × 0x141000 = 0x0504_0000` (81 MiB reserved, lazily
committed — M11); `KERNEL_HEAP_BASE = 0x0506_0000` (wasm-page aligned, page 1286);
`KSM_INITIAL_PAGES = 1302` (`0x516_0000` ≈ 84.6 MiB — the fixed map + 1 MiB
initial kernel heap); `KSM_DEFAULT_MAX_PAGES = 16384` (1 GiB, host-tunable before
creation; wasm32 hard cap 4 GiB).

**Growth rules:** the ABI regions (KCTL, endpoint table, doorbell, transport pool)
are **fixed at boot and never grow, move, or shrink** — their addresses are
compile-time ABI constants. Only the kernel heap grows (via `memory.grow` of the
shared memory; offsets below the growth point never move — M10 VERIFIED). Rationale
for fixed arenas: (1) growing a region would invalidate offsets held in in-flight
requests and require a renegotiation protocol — a whole class of races eliminated
by not growing; (2) reserved-but-untouched pages cost ~KiB RSS (M11), so generous
fixed capacity is nearly free; (3) "no unbounded queues" (directive §8) is
satisfied by construction. Capacity changes require an ABI major/minor version
bump (§20.3).

**Kernel-reserved regions:** the kernel heap `[KERNEL_HEAP_BASE, …)` is reserved
for kernel code and data. Bridges MUST NOT write there (§19 trust model defines
what is enforced vs. trusted). The pad region is reserved for future ABI-controlled
structures (versioned).

### 5.3 KCTL — kernel control block (offset 0, 64 B used)

| Offset | Size | Type | Field | Writer / reader | Meaning |
|---|---|---|---|---|---|
| `+0x00` | 8 | u64 | `magic` | kernel W (once) / all R | `0x3145_4350_4152_4143` (ASCII `"CARAPCE1"` LE) |
| `+0x08` | 4 | u32 | `abi_version` | kernel W (once) / all R | `(major << 16) \| minor`; v1.0 → `0x0001_0000` |
| `+0x0C` | 4 | u32 | `generation` | kernel W (per boot) / all R | boot generation; incremented every kernel (re)start |
| `+0x10` | 4 | u32 | `kernel_state` | kernel W / all R | `0=BOOTING, 1=RUNNING, 2=DRAINING, 3=STOPPED` |
| `+0x14` | 4 | u32 | `kernel_caps` | kernel W (once) / all R | capability bitmask (§20.1) |
| `+0x18` | 4 | u32 | `max_endpoints` | kernel W (once) / all R | 64 |
| `+0x1C` | 4 | u32 | `endpoint_stride` | kernel W (once) / all R | `0x0014_1000` |
| `+0x20` | 4 | u32 | `transport_base` | kernel W (once) / all R | `0x0001_0000` |
| `+0x24` | 4 | u32 | `kernel_heap_base` | kernel W (once) / all R | `0x0506_0000` |
| `+0x28` | 8 | u64 | `kernel_heartbeat_ms` | kernel W (≥ every 250 ms) / all R | `Date.now()` at last pump pass |
| `+0x30` | 4 | u32 | `doorbell_offset` | kernel W (once) / all R | `0x0000_2000` |
| `+0x34` | 4 | u32 | `ksm_pages_current` | kernel W / host R | current `memory.buffer.byteLength / 65536` |
| `+0x38` | 4 | u32 | `ksm_pages_max` | kernel W (once) / all R | 16384 (or host-configured) |
| `+0x3C` | 4 | u32 | `reserved` | — | 0 |

Ordering: `magic`, `abi_version`, `generation`, and all once-written constants are
stored by the kernel with SeqCst stores **before** `kernel_state` transitions to
`RUNNING` (single release point); readers acquire-load `kernel_state` first and
only then read the rest (§15.4 field table).

### 5.4 Endpoint table (offset `0x1000`, 64 slots × 64 B)

The discovery record for each endpoint. Written by the kernel at endpoint
allocation; read by the bridge at its bootstrap; never modified while the endpoint
is live.

| Offset | Size | Type | Field | Meaning |
|---|---|---|---|---|
| `+0x00` | 8 | u64 | `magic` | `0x3130_5345_5052_4143` (ASCII `"CARPES01"` LE) |
| `+0x08` | 4 | u32 | `ep_base` | absolute KSM offset of the endpoint region |
| `+0x0C` | 4 | u32 | `pid` | owning process id (§8.2) |
| `+0x10` | 4 | u32 | `generation` | endpoint generation (guards stale bridges after process recycling) |
| `+0x14` | 4 | u32 | `state` | `0=FREE, 1=ALLOCATED, 2=RUNNING, 3=DRAINING, 4=DEAD` |
| `+0x18` | 4 | u32 | `ep_caps` | bit0 `SYNC_ALLOWED`, bit1 `ASYNC_ALLOWED`, bit2 `INGRESS` (kernel-internal proxy endpoint, §13) |
| `+0x1C` | 4 | u32 | `reserved` | 0 |
| `+0x20` | 8 | u64 | `owner_tag` | diagnostic: host worker-instance id |
| `+0x28` | 8 | u64 | `reserved` | 0 |
| `+0x30` | 8 | u64 | `reserved` | 0 |
| `+0x38` | 8 | u64 | `reserved` | 0 |

The kernel writes a slot with SeqCst stores ending with `state` (the release
field); the bridge acquire-loads `state`, then the rest. A bridge whose endpoint
slot shows `state ≠ RUNNING` or a `generation` mismatch fails fast with
`ERR_NODINE_ENDPOINT_STATE` (§14.4).

### 5.5 Endpoint region (per process; base = `transport_base + endpoint_index × EP_STRIDE`)

All offsets below are **endpoint-relative**. Request/result references
(`tx_off`, `result_off`) are relative to the TX / RX / sync-result sub-region
bases, so validation is a bound check against the constants below (§19).

| Offset | Size | Sub-region | Notes |
|---|---|---|---|
| `+0x0000` | 64 B | **Endpoint header** | below |
| `+0x0040` | 16 B | **Submit-ring header** | below |
| `+0x0050` | 16 B | **Completion-ring header** | below |
| `+0x0060` | 64 B | **Sync mailbox** | §6.3 |
| `+0x00A0` | 96 B | pad/reserved | future header fields (versioned) |
| `+0x0100` | `0x4_0000` (256 KiB) | **TX arena** | producer-owned request payloads |
| `+0x4_0100` | `0x4_0000` (256 KiB) | **RX arena** | kernel-owned async completion payloads |
| `+0x8_0100` | `0x4_0000` (256 KiB) | **Sync-result area** | kernel-written, single-occupancy (§11.4) |
| `+0xC_0100` | `0x4_0000` (4096 × 64 B) | **Submit-ring slots** | `RequestRecord[4096]` |
| `+0x10_0100` | `0x4_0000` (4096 × 64 B) | **Completion-ring slots** | `CompletionRecord[4096]` |
| `+0x14_0100` | `0xF00` | pad to `EP_STRIDE` | — |

`EP_STRIDE = 0x0014_1000`. Constants: `TX_SIZE = RX_SIZE = SYNCRES_SIZE =
0x4_0000`; `RING_SLOTS = 4096` (power of two); `SLOT_SIZE = 64`.

**Endpoint header** (`+0x0000`, 64 B):

| Offset | Size | Type | Field | Writer | Meaning |
|---|---|---|---|---|---|
| `+0x00` | 8 | u64 | `ep_magic` | kernel once | `0x3130_5045_5052_4143` (`"CARPEP01"` LE) |
| `+0x08` | 4 | u32 | `ep_pid` | kernel once | owning pid |
| `+0x0C` | 4 | u32 | `ep_generation` | kernel once | matches endpoint-table generation |
| `+0x10` | 4 | u32 | `ep_state` | kernel | `0=FREE,1=ALLOCATED,2=RUNNING,3=DRAINING,4=DEAD` |
| `+0x14` | 4 | u32 | `ep_flags` | kernel | reserved, 0 |
| `+0x18` | 8 | u64 | `ep_heartbeat_ms` | **bridge** (≥ every 250 ms of event-loop progress) | liveness (§21.4) |
| `+0x20` | 32 B | — | reserved | — | 0 |

**Submit-ring header** (`+0x0040`):

| Offset | Size | Type | Field | Writer / reader | Meaning |
|---|---|---|---|---|---|
| `+0x40` | 4 | u32 | `sr_head` | **kernel** store / bridge load | next slot index (mod 2³² counter) the kernel will consume |
| `+0x44` | 4 | u32 | `sr_tail` | **bridge** store / kernel load | first not-yet-enqueued slot index |
| `+0x48` | 4 | u32 | `sr_sync_pending` | bridge `add` / kernel `load`,`add` | count of unconsumed `MODE_SYNC` requests (priority hint, §10.3) |
| `+0x4C` | 4 | u32 | reserved | — | 0 |

**Completion-ring header** (`+0x0050`):

| Offset | Size | Type | Field | Writer / reader | Meaning |
|---|---|---|---|---|---|
| `+0x50` | 4 | u32 | `cr_head` | **bridge** store / kernel load | next completion index the bridge will retire |
| `+0x54` | 4 | u32 | `cr_tail` | **kernel** store / bridge load | first not-yet-written completion index |
| `+0x58` | 4 | u32 | `cr_seq` | **kernel** store+notify / bridge `waitAsync` | completion doorbell: incremented by the kernel after each `cr_tail` store |
| `+0x5C` | 4 | u32 | reserved | — | 0 |

### 5.6 Kernel heap

`[KERNEL_HEAP_BASE, ksm_end)` is the kernel's Rust-allocator domain (VFS nodes,
process table, OFD table, kernel stacks). It is **not** part of the ABI surface:
nothing in it is referenced by ABI records (all ABI references point into
endpoint-owned sub-regions). The heap grows by `memory.grow` (M10). Kernel-heap
exhaustion surfaces as `ENOMEM` (−12, §14) from the failing op — an honest
resource error, never a hang.

---

## 6. Core data structures (byte-level)

All structs are `#[repr(C)]`, little-endian, 8-byte aligned, 64 bytes. Rust
definitions: §24.

### 6.1 `RequestRecord` (64 B) — one per ring slot; written by the producer only

| Offset | Size | Type | Field | Meaning |
|---|---|---|---|---|
| `+0x00` | 8 | u64 | `req_id` | per-endpoint monotonic id (§8.1) |
| `+0x08` | 4 | u32 | `op` | operation id (§7) |
| `+0x0C` | 4 | u32 | `flags` | §8.4 |
| `+0x10` | 4 | u32 | `tx_off` | payload offset **within this endpoint's TX arena**; 8-aligned; 0 if `tx_len = 0` |
| `+0x14` | 4 | u32 | `tx_len` | payload length in bytes (0 = no payload) |
| `+0x18` | 8 | u64 | `a0` | op-defined argument (§7) |
| `+0x20` | 8 | u64 | `a1` | op-defined argument |
| `+0x28` | 8 | u64 | `a2` | op-defined argument |
| `+0x30` | 8 | u64 | `a3` | op-defined argument |
| `+0x38` | 8 | u64 | `a4` | op-defined argument |

Argument encoding conventions (per-op table in §7 fixes the meaning):

- `IMM` — an integer placed zero-extended (unsigned) or sign-extended (i64
  positions) into the u64 slot.
- `PATH` — the path string is the TX payload: bytes at `tx_off..tx_off+tx_len`,
  UTF-8, **without** NUL terminator, length ≤ `PATH_MAX = 4096`.
- `PATH2` — a second path packed into the same TX payload after the first,
  separated by one NUL: `[path_a][0x00][path_b]` (used by `OP_RENAME`); POSIX
  paths cannot contain NUL, so the packing is unambiguous.
- `FD` — per-process fd (u32, zero-extended).
- Positions: `a2` is i64 wherever an op takes a file position (−1 = "use the fd's
  current offset", matching Node's `read()`/`write()` binding convention —
  VERIFIED `poc/p1-poc/worker.mjs:1575-1614`).

### 6.2 `CompletionRecord` (64 B) — one per completion-ring slot; written by the kernel only

| Offset | Size | Type | Field | Meaning |
|---|---|---|---|---|
| `+0x00` | 8 | u64 | `req_id` | identifies the completed request (§8.1); for unsolicited event completions: the watch id (§7 stubs) |
| `+0x08` | 4 | u32 | `op` | echo of the request's op (validation/diagnostics) |
| `+0x0C` | 4 | i32 | `status` | `0` = success; `< 0` = error (uv errno or `NODINE_*`, §14) |
| `+0x10` | 4 | u32 | `result_kind` | `0=NONE, 1=INLINE, 2=RX_ARENA, 3=SYNCRES` (§6.4) |
| `+0x14` | 4 | u32 | `result_len` | payload bytes (0 for `NONE`; ≤ 16 for `INLINE`) |
| `+0x18` | 4 | u32 | `result_off` | offset within the named area (8-aligned) |
| `+0x1C` | 4 | u32 | `cflags` | bit0 `EVENT` (unsolicited, not a request completion); rest 0 |
| `+0x20` | 16 | u8[16] | `inline` | inline result bytes (`INLINE` only) |
| `+0x30` | 16 | — | reserved | 0 |

A completion identifies its request by `req_id`; the bridge additionally checks
the `op` echo and that the id is currently outstanding (§19.5 stale-completion
rule). Ordering guarantees: §12.4.

### 6.3 Sync mailbox (64 B, endpoint-relative `+0x0060`) — kernel-written completion cell for `MODE_SYNC`

Embeds the same completion fields as §6.2 plus the doorbell/state words the
runtime worker waits on:

| Offset | Size | Type | Field | Writer | Meaning |
|---|---|---|---|---|---|
| `+0x00` | 4 | u32 | `sm_seq` | **kernel** store+notify / bridge `wait` | sync completion counter; incremented by the kernel on every sync completion or abandon-reset |
| `+0x04` | 4 | u32 | `sm_state` | both, CAS | `0=IDLE, 1=SUBMITTED, 2=COMPLETED, 3=ABANDONED` (§11.5) |
| `+0x08` | 4 | u32 | `sm_op` | kernel | op echo |
| `+0x0C` | 4 | u32 | reserved | — | 0 |
| `+0x10` | 8 | u64 | `sm_req_id` | kernel | completed request id |
| `+0x18` | 4 | i32 | `sm_status` | kernel | as §6.2 `status` |
| `+0x1C` | 4 | u32 | `sm_kind` | kernel | `0=NONE, 1=INLINE, 3=SYNCRES` — **never** `RX_ARENA` (§11.2) |
| `+0x20` | 4 | u32 | `sm_off` | kernel | offset within the sync-result area (8-aligned) |
| `+0x24` | 4 | u32 | `sm_len` | kernel | payload bytes |
| `+0x28` | 16 | u8[16] | `sm_inline` | kernel | inline result bytes |
| `+0x38` | 8 | — | reserved | — | 0 |

### 6.4 Result representation

- **INLINE (kind 1):** ≤ 16 bytes copied into the record itself (`fd`, byte
  counts, probe echoes). No arena touch, no retire dependency. Inline size 16 B
  covers every scalar result in the v1 op set; the 144-byte stat result does not
  fit and deliberately uses an area (measured transport cost is dominated by the
  round trip, M1; a 144 B shared-memory copy is noise — §23).
- **RX_ARENA (kind 2):** async payload results; `{result_off, result_len}` point
  into the endpoint's RX arena. The kernel bump-allocates 8-aligned chunks in
  completion (dequeue) order; the consumer frees in retire order (§9.4).
- **SYNCRES (kind 3):** sync payload results; points into the sync-result area,
  which has single-occupancy semantics (§11.4) — it is always empty when the
  kernel begins writing a sync result.
- **NONE (kind 0):** status-only completion (`close`, `mkdir`, …).

Maximum payload result sizes are **caller-declared** (op args bound them:
`READ`'s `a1`, `READDIR`'s `a2`), so every buffer is sized at endpoint creation
and the kernel can always predict its space needs (§17). All sync-mode ops are
bounded by `SYNCRES_SIZE − 64` (header reservation, consistent with §7.3/§11.6 —
erratum P2-005-003: this line previously said `−8`, contradicting the normative
`−64`; the shipped transport implements −64); the bridge chunks larger user-level
operations
(§11.6) exactly as upstream Node itself loops internally
(`vendor/node-v26.13.0/lib/fs.js:660-676` — readFileSync loops 8 KiB reads when
size is unknown — VERIFIED).

---

## 7. Operation table

Op ids are u32, stable within an ABI major version (§20.3). "Args" lists only the
used fields; unused fields MUST be 0 (else `NODINE_EINVAL_REQUEST`). Every op is
legal in both modes unless noted; mode changes only completion delivery.

### 7.1 Control & capability

| Op (hex) | Name | Args | Result | Semantics |
|---|---|---|---|---|
| `0x0001` | `OP_ABI_PROBE` | `a0` = bridge `abi_version` | INLINE u64 `[0]=kernel abi_version, [8]=kernel_caps` | No side effects. Kernel completes with `status=0` iff `a0.major == kernel.major` and `kernel.minor ≥ a0.minor`; else `status=NODINE_EABI_VERSION`. Used at endpoint bring-up and after any suspected kernel restart (§21.3). |
| `0x0002` | `OP_CAPS_QUERY` | — | INLINE u64 `[0]=kernel_caps` (§20.1) | Returns the capability bitmap. |

### 7.2 Process lifecycle (stubs where marked)

| Op | Name | Args | Result | Semantics |
|---|---|---|---|---|
| `0x0010` | `OP_PROC_READY` | `a0` = bridge `abi_version`, `a1` = bridge flags (0) | status | Bridge → kernel at endpoint bring-up (async). Kernel transitions the endpoint `ALLOCATED → RUNNING`. Version re-checked as in `OP_ABI_PROBE`. |
| `0x0011` | `OP_PROC_EXIT` | `a0` = exit code (i64) | status | Bridge → kernel on process exit (async). Kernel drains this endpoint (§21.2), reclaims all fds/OFDs of the pid, transitions the endpoint to `DEAD`, and notifies the host control plane for worker recycling. |
| `0x0012` | `OP_PROC_SPAWN` | *reserved* (0) | — | **Stub.** Reserved for Phase 3 `child_process`; submitting it in v1.0 returns `NODINE_ENOOP`. |
| `0x0013` | `OP_PROC_KILL` | `a0` = pid, `a1` = signal | status | **Stub** (Phase 3). |
| `0x0014` | `OP_PROC_WAIT` | `a0` = pid | — | **Stub** (Phase 3). |

### 7.3 Filesystem (the Phase 2 op set)

Paths are TX payloads (`PATH`/`PATH2`, §6.1); `fd` args are per-process (§8.3).
Open flags/mode values are Linux `O_*`/mode bits exactly as the Node binding
passes them (VERIFIED `poc/p1-poc/worker.mjs:1516` `O = {RDONLY:0, WRONLY:1,
RDWR:2, CREAT:64, EXCL:128, TRUNC:512, APPEND:1024}`; `stringToFlags` maps Node's
string flags to these — `vendor/.../lib/internal/fs/utils.js:807-848`).

| Op | Name | Args | Result | Semantics (kernel side) |
|---|---|---|---|---|
| `0x0100` | `OP_OPEN` | `PATH`; `a0` = flags, `a1` = mode | INLINE u32 fd | Resolve path; validate flags/mode; create open-file description (OFD, u64 kernel id); install lowest free per-process fd; result = fd. Errors: `ENOENT/ENOTDIR/EEXIST/EISDIR/EACCES…` per POSIX/uv. |
| `0x0101` | `OP_CLOSE` | `a0` = fd | NONE | Remove fd→OFD mapping; OFD refcount-- (last ref closes the description). `EBADF` if not open. Idempotence: closing an already-closed fd is `EBADF` (POSIX). |
| `0x0102` | `OP_READ` | `a0` = fd, `a1` = `len_max`, `a2` = pos (i64, −1 = fd offset) | AREA: ≤ `len_max` bytes; `status`-only on EOF (kind NONE, len 0) | Read `min(len_max, avail)` bytes from the file into the result area; `len_max ≤ RX_SIZE−64` (async) / `≤ SYNCRES_SIZE−64` (sync); advance fd offset if `pos = −1`. |
| `0x0103` | `OP_WRITE` | `a0` = fd, `a1` = pos (i64, −1 = fd offset/append), TX payload = data | INLINE u32 n | Write `tx_len` bytes from the TX arena; `tx_len ≤ TX_SIZE−64`. Append semantics when the OFD has `O_APPEND`. Result = bytes written (always = `tx_len`; partial writes are retried internally — divergence from POSIX partial-write is deliberately not observable, matching uv's loop). |
| `0x0104` | `OP_STAT` | `PATH` | AREA 144 B | Stat by path. `ENOENT` when missing (never silently 0 — the bridge implements Node's `throwIfNoEntry`, VERIFIED `worker.mjs:1798-1808`). |
| `0x0105` | `OP_LSTAT` | `PATH` | AREA 144 B | Same, no symlink dereference (v1 VFS has no symlinks; op exists for ABI completeness). |
| `0x0106` | `OP_FSTAT` | `a0` = fd | AREA 144 B | Stat the OFD's file. `EBADF` if invalid. |
| `0x0107` | `OP_MKDIR` | `PATH`; `a0` = mode | NONE | Create directory. `EEXIST/ENOENT/ENOTDIR` per POSIX. |
| `0x0108` | `OP_RMDIR` | `PATH` | NONE | Remove empty directory. `ENOTEMPTY/ENOENT`. |
| `0x0109` | `OP_UNLINK` | `PATH` | NONE | Remove file. `ENOENT`. Open fds keep the file alive (OFD holds a vnode ref — POSIX unlink-while-open). |
| `0x010A` | `OP_RENAME` | `PATH2` (`old\0new`) | NONE | Rename; `ENOENT/ENOTEMPTY/EISDIR/ENOTDIR` per POSIX. |
| `0x010B` | `OP_READDIR` | `PATH`; `a0` = cookie (u64, 0 = start), `a1` = `max_entries`, `a2` = `max_bytes` | AREA: batch (below) | Stateless directory iteration: returns up to `max_entries` entries / `max_bytes` payload bytes starting at `cookie`, plus the next cookie. `max_bytes ≤ RX_SIZE−64` / `≤ SYNCRES_SIZE−64`. The cookie is an opaque u64 (v1.0.2: dentry sequence number — erratum P2-005-003; opacity is part of the contract). End of directory: batch with `count = 0`. |
| `0x010C` | `OP_FTRUNCATE` | `a0` = fd, `a1` = len | NONE | **Stub** — reserved (P2-005 optional); `NODINE_ENOOP` in v1.0. |
| `0x010D` | `OP_ACCESS` | `PATH`; `a0` = mode | NONE | **Stub** — reserved; `NODINE_ENOOP` in v1.0. |

**Stat result encoding (144 B = 18 × f64, or 18 × i64 with `FLAG_STAT_BIGINT`)**
— field order is exactly Node's `getStatsFromBinding` (VERIFIED
`vendor/node-v26.13.0/lib/internal/fs/utils.js:763-786`; mirrored by the PoC's
`statArrayFor` `worker.mjs:1526-1544`):

```
[0] dev   [1] mode  [2] nlink [3] uid  [4] gid  [5] rdev [6] blksize [7] ino
[8] size  [9] blocks
[10] atime_sec [11] atime_nsec   [12] mtime_sec [13] mtime_nsec
[14] ctime_sec [15] ctime_nsec   [16] birthtime_sec [17] birthtime_nsec
```

The bridge hands these bytes to Node as a `Float64Array(18)` / `BigInt64Array(18)`
view — zero conversion (§28 worked example).

**Readdir batch encoding** (in the result area):

```
u32 count                       number of entries in this batch
u64 next_cookie                 cookie for the next batch (undefined at EOF; use count==0 as EOF)
repeat count × {
  u8   kind                     UV_DIRENT_*: 0=UNKNOWN,1=FILE,2=DIR,3=LINK,4=BLOCK,5=CHAR,6=FIFO,7=SOCKET
  u8   pad[3]
  u32 name_len
  u8   name[name_len]           UTF-8, no NUL, no '/' separator
}
```

(8-byte header, 8-aligned entries' name arrays padded to 8 — the kernel pads each
batch to keep every batch 8-aligned from the area base.)

### 7.4 Watchers (stubs) and cancellation

| Op | Name | Args | Result | Semantics |
|---|---|---|---|---|
| `0x0110` | `OP_WATCH_START` | `PATH`; `a0` = flags | INLINE u64 watch_id | **Stub** — reserved (P2-005+). Events, when implemented, are unsolicited `CompletionRecord`s with `cflags.EVENT=1`, `req_id = watch_id`, `op = OP_WATCH_EVENT` (`0x0112`, reserved). `NODINE_ENOOP` in v1.0. |
| `0x0111` | `OP_WATCH_STOP` | `a0` = watch_id | NONE | **Stub**. |
| `0x00F0` | `OP_CANCEL` | `a0` = req_id | status | Best-effort cancellation (§18). Async mode only. |

### 7.5 Op-id allocation plan

`0x0001–0x000F` control · `0x0010–0x001F` process · `0x00F0–0x00FF` meta
(cancel) · `0x0100–0x011F` filesystem v1 · `0x0112` watch-event (reserved) ·
`0x0120–0x012F` **fs deferred surface, v1.0-reserved** (0x0120 `OP_FSYNC` —
v1.0-defined, P2-008 implementation, journal barrier per §13.5; 0x0121
`OP_TRUNCATE` full; 0x0122 `OP_CHMOD`; 0x0123 `OP_UTIMES`; 0x0124 `OP_LINK`;
0x0125 `OP_SYMLINK`; 0x0126 `OP_READLINK`; 0x0127 `OP_REALPATH`; 0x0128
`OP_MKDTEMP`; 0x0129 `OP_COPYFILE`; 0x012A `OP_STATFS` — group bit `FS_V1`
gates the whole range; absent bit ⇒ `ENOSYS`, never silent) · `0x0130–0x013F`
**v1.1 fused one-shots, reserved** (`OP_FUSED_READ_FILE`,
`OP_FUSED_WRITE_FILE`, … — E's G8: open+fstat+read+close in one request;
adopt after P2-009 measures the §11.6 chunk overhead) · `0x0140`
**`OP_INGRESS_READ` — Phase-2 ingress op (§7.6)** · `0x0141`
`OP_INGRESS_RESOLVE` (v1.1-reserved; only if a workload provably needs
SW-side resolution — one resolver instance stays the rule, §13) ·
`0x0200–0x02FF` reserved for Phase 3 (proc IPC, threads) · `0x0300–0x03FF`
reserved for vTCP (Phase 5) · `0x1000+` unallocated. New ops within a major
version = minor bump (§20.3).

### 7.6 Ingress ops (Phase 2 — module-serving set; v1.0.1)

| Op | Name | Args | Result | Semantics |
|---|---|---|---|---|
| `0x0140` | `OP_INGRESS_READ` | `PATH` (percent-decoded VFS path, rule below); `a0` = max_len | AREA: file bytes (≤ min(max_len, RX_SIZE−64)) | Exact-path read for the Service Worker's module serving (§13; ESM Strategy 3). **URL→path rule:** `path = percentDecode(url.pathname)` — Node-consistent shadowing (a file literally named `my%20file.mjs` is served only under its doubly-encoded URL; a raw lone `%` or invalid sequence ⇒ `ENOENT`, mapped by the SW to a 404; ESM_STRATEGY §6.2 v1.0.1 documents the encoding). **The SW NEVER guesses extensions or directories** — Node's own resolver is the single resolution authority and itself produces `ERR_MODULE_NOT_FOUND`/`ERR_UNSUPPORTED_DIR_IMPORT` (vendored `resolve.js:229`); any non-exact path ⇒ `ENOENT`. `EISDIR` for directories. Read-only, module-serving-scoped: `max_len` ≤ 1 MiB per request; larger reads belong to the async class or the runtime worker's normal fs ops. Capability-gated: endpoint carries `ep_caps = INGRESS\|ASYNC`. |
| `0x0141` | `OP_INGRESS_RESOLVE` | — | — | **Reserved (v1.1)** — not defined in v1.0.1. If a future workload requires SW-side resolution, it MUST execute the same vendored `resolve.js` in a JS context (one resolver instance — ESM_STRATEGY §6.5), never a second implementation. |

These ops exist so the recommended ESM strategy has an ABI-legal data path
(adversarial G2b C1: ESM_STRATEGY required SW-originated reads while the v1.0
op table said "Phase 2: none in the fs set" — a freeze-blocking conflict, now
resolved). The end-to-end module-serving path (SW → MessageChannel envelope →
kernel driver → ingress endpoint → kernel → VFS → back) is re-proven on a real
module graph in P2-004 with **zero module bytes in the SW's heap before the
fetch** (replacing the spike's posted-VFS-copy evidence).

---

## 8. Identifiers, flags, and versioning

### 8.1 Request identifiers — u64, monotonic per endpoint

- One counter per endpoint, incremented by 1 for every request submitted on
  **either** lane (sync and async share the counter; ids are unique per endpoint
  across lanes).
- **Reuse-after-wraparound cannot corrupt outstanding requests.** The counter is
  u64: wraparound requires 2⁶⁴ submissions — at 10⁹ requests/sec that is 584
  years [INFERRED, arithmetic]. The protocol is nonetheless wraparound-safe by
  construction: an id may be **re-issued only after its completion has been
  retired** (ring-slot retirement, §10.1/§12.3, and the bridge's outstanding-id
  table), and a completion is delivered **exactly once** per request (kernel
  invariant, §12.4). Hence a duplicate completion for id X can exist only if X's
  slot was retired while a completion was still in flight — impossible, because
  retirement requires observing that completion, which consumes it. Wraparound
  would therefore re-issue ids only in the same retire-before-reuse discipline;
  outstanding requests can never be confused with a successor carrying the same
  id. (Belt-and-braces: the bridge validates `op` echo + outstanding-table hit,
  §19.5 — **except** unsolicited `cflags.EVENT` completions (watch
  notifications), which have no outstanding-table entry by design; v1.0.1,
  adversarial F-B10.)
- The kernel does **not** interpret ids beyond echo and cancellation lookup.

### 8.2 Process identifiers — u32, kernel namespace

- pid `0` = the kernel itself (never a process). pid `1` = the first Node process
  (init-shaped). pids are assigned monotonically per boot from 1 and are **never
  reused within a boot generation** (recycled *endpoint slots* get a new
  generation; a new process gets a new pid — no pid-confusion across recycled
  workers, ADR-0003 full-teardown recycling).
- `MAX_PROCESSES` is bounded by `MAX_ENDPOINTS` (one endpoint per process in
  Phase 2). pid → endpoint resolution is kernel-internal; the ABI carries pids
  only as *arguments* (`OP_PROC_KILL` etc.), never as request metadata: **the
  kernel derives the calling process from the endpoint the request arrived on**
  (ownership by construction; a request cannot claim another pid).

### 8.3 File-descriptor identifiers — two namespaces

- **Kernel namespace (`kfd`, u64):** the OFD id. Monotonic, never reused within a
  boot → use-after-close is detectable and stale references never resolve.
- **Per-process table (fd, u32):** POSIX-shaped small ints; `0/1/2` = stdio
  (kernel-owned; `close` on them is a no-op success — matching the PoC,
  `worker.mjs:1702-1706`), first allocatable fd = 3. fd numbers are reused after
  close (POSIX behavior). The table is part of the *process record* (kernel
  state), so all endpoints of one process (future thread workers) share it.
- The ABI's `FD` args are **per-process fds**; the kernel validates them against
  the calling process's table. Cross-process fd passing is not in v1 (Phase 3
  `fork`/IPC). `OP_READ` on a fd opened write-only → `EBADF` (uv/Node semantics —
  the PoC mirrors, `worker.mjs:1575-1589`).
- Per-process fd cap (v1.0.1, adversarial delta-5 adjudication; VFS_DESIGN
  §2.7): the kernel enforces `FD_MAX_PER_PROC` (default 1024, host-tunable);
  exhaustion ⇒ `EMFILE` (per-process); the global table cap ⇒ `ENFILE` — both
  explicit, never silent degradation.
- stdio device OFDs (ADR-0008, v1.0.1): fds 0/1/2 are kernel-owned device
  OFDs — `OP_WRITE` on fd 1 routes to the console sink (the `console.log` hot
  path, E's G9: upstream calls `writeBuffer(1, …, ctx)` and reports errors via
  `ctx.errno`, never a throw from the binding), fd 2 to the console error
  sink, fd 0 to the terminal input source; ops on them never touch the VFS.

### 8.4 Request flags (u32)

| Bit | Mask | Name | Meaning |
|---|---|---|---|
| 0 | `0x0000_0001` | `FLAG_MODE_SYNC` | Endpoint class 1 (§11). 0 = class 2 (§12). |
| 1 | `0x0000_0002` | `FLAG_STAT_BIGINT` | `OP_STAT/LSTAT/FSTAT`: result is 18 × i64 instead of 18 × f64. |
| 2–31 | — | reserved | MUST be 0. Any set bit → `NODINE_EINVAL_REQUEST` (forward-compat is by version, not by unknown-flag tolerance — §20.3). |

### 8.5 ABI version

`abi_version = (major:u16 << 16) | (minor:u16)`. v1.0 = `0x0001_0000`. Negotiation
and compatibility rules: §20.

---

## 9. Data arenas, ownership, and buffer lifetime

### 9.1 Ownership summary (single-writer-per-cell invariant)

| Region | Single writer | Readers | When readable |
|---|---|---|---|
| KCTL once-fields | kernel (boot) | everyone | after `kernel_state` ≥ RUNNING |
| `kernel_heartbeat_ms`, `kernel_state` | kernel (repeatedly) | everyone | anytime (advisory) |
| Endpoint table slot | kernel (allocation) | bridge (boot), host | after slot `state` store |
| TX arena bytes | **producer** (bridge) | kernel | from `sr_tail` release-store until the producer reuses the bytes (retire-gated) |
| Submit-ring slot bytes | **producer** (bridge) | kernel | same as its TX payload window |
| `sr_tail`, `sr_sync_pending` | **producer** | kernel | per §15.4 |
| `sr_head` | **kernel** | producer (full-check) | per §15.4 |
| RX arena bytes | **kernel** | consumer (bridge) | from `cr_tail` release-store until the kernel reuses (retire-gated) |
| Completion-ring slot bytes | **kernel** | consumer | same window |
| `cr_tail`, `cr_seq` | **kernel** | producer/consumer | per §15.4 |
| `cr_head` | **consumer** (bridge) | kernel | per §15.4 |
| Sync-result area bytes | **kernel** | consumer (bridge) | from `sm_seq` release-store until the next sync submit |
| Sync mailbox completion fields | **kernel** | consumer | from `sm_seq` release-store |
| `sm_state` | both via CAS | both | §11.5 |
| `ep_heartbeat_ms` | **bridge** | kernel | advisory |

No cell is ever written by two agents: every byte in the KSM has exactly one
owning writer at any time (transfers are explicit and ordered — e.g. a TX slot's
ownership stays with the producer forever; the kernel only reads). This is the
single-writer-per-cell invariant; it is what makes the non-atomic payload bytes
race-free under the atomics-based index discipline (§15.3).

### 9.2 TX arena (request payloads) — producer-owned

- Size `TX_SIZE = 0x4_0000` (256 KiB), fixed. The **last `TX_SYNC_RESERVE =
  0x2000` (8 KiB) bytes are reserved for `MODE_SYNC` payloads** (paths; the
  reserve guarantees a sync submit never fails on arena space, §17.1).
- Allocation inside the arena is a **producer-internal concern** (the reference
  bridge uses a segregated free-list; a bump allocator with retire-driven free
  also works). The ABI constrains only: (a) `tx_off` 8-aligned and
  `tx_off + tx_len ≤ TX_SIZE` (kernel-validated, §19.2); (b) **lifetime** — bytes
  referenced by an outstanding request MUST NOT be modified until the producer
  observes that request's completion (the kernel may read them at any point up to
  its completion store).
- Because the sync lane can complete out of submission order (priority, §10.3),
  TX reclamation is naturally out-of-order; the free-list handles it. (Rationale
  for not specifying the allocator: it is single-threaded producer-local state,
  invisible to the kernel and to the ABI's correctness proofs.)

### 9.3 RX arena (async completion payloads) — kernel-owned

- Size `RX_SIZE = 0x4_0000` (256 KiB), fixed. The kernel bump-allocates 8-aligned
  chunks in **dequeue order** (which, per endpoint, equals async submission
  order, §12.4); the consumer frees FIFO in **retire order** (also submission
  order). Allocation order = free order ⇒ the arena is a simple byte-ring:
  kernel advances a private tail, reads the consumer's `cr_head` to compute free
  space.
- An async payload result is written only if `RX free ≥ result_len + 64`;
  otherwise the kernel defers the whole op (does not dequeue it — §17.2). Deferral
  is latency, never deadlock (§16).
- Lifetime: RX bytes referenced by a completion are stable until the consumer
  retires that completion (advances `cr_head` past its ring slot). After retire,
  the kernel may reuse the space; the consumer MUST have finished copying before
  its `cr_head` store (§15.3 L3).
- Per-record RX ledger (v1.0.1, adversarial F-B6): every completion returning
  arena payload carries its exact `len`; RX occupancy is accounted as the sum
  of un-retired completions' `len` values (a byte ledger) — never a slot count
  or a single high-water mark. Mixed inline/payload streams MUST retire arena
  space exactly (P2-004 asserts interleaved inline + max-payload completions
  over ≥ 4096-deep rings wrap the arena with zero drift).

### 9.4 Sync-result area — kernel-written, single-occupancy

- Size `SYNCRES_SIZE = 0x4_0000` (256 KiB), fixed, dedicated to `MODE_SYNC`
  results. **Single-occupancy:** at most one sync request is outstanding per
  endpoint (§11.1), and the previous sync request was fully retired (same thread,
  program order) before the next was submitted ⇒ the area is **always empty when
  the kernel begins writing a sync result**. The kernel therefore never checks
  space for sync results and can never be blocked by consumer state on the sync
  path — the load-bearing deadlock-freedom property (§16.2).
- Lifetime: bytes are stable from the `sm_seq` store until the worker's next sync
  submission (its CAS `IDLE→SUBMITTED` is the release point proving the previous
  result was consumed) or endpoint death.

### 9.5 Ring slots

Ring-slot bytes belong to the ring's producer until the tail passes them, and to
the consumer's read window until the head passes them. The index discipline
(§10.1) guarantees a slot is never written while it is inside the consumer's
un-read window (power-of-two capacity + occupancy bound).

### 9.6 Copy accounting (honest two-copy design)

- **Reads:** kernel copies VFS node bytes → result area (1 kernel-side memcpy);
  bridge copies result area → Node `Buffer`/`Uint8Array` on the JS heap (1
  bridge-side copy). Node `Buffer`s are engine-heap objects; the kernel cannot
  write into them directly. Total: 2 copies per read payload.
- **Writes:** bridge copies Node `Buffer` → TX arena; kernel copies TX arena →
  VFS node. 2 copies.
- Budget and measurement plan: §23. Zero-copy refinements (lending kernel VFS
  pages read-only, or Node `Buffer`s over KSM views with retire-safe re-issue)
  are **versioned future extensions** (§20.3), not fallbacks — the v1 semantics
  are complete and closed.

---

## 10. Ring mechanics

### 10.1 SPSC index pair (per ring)

Each ring is single-producer/single-consumer with a u32 **wraparound index pair**
(`sr_head`/`sr_tail`, `cr_head`/`cr_tail`):

- Slot count `RING_SLOTS = 4096` = 2¹² (power of two); slot address =
  `slots_base + (index & (RING_SLOTS−1)) × 64`.
- The indices are **monotonic mod 2³²** (not masked in memory). Occupancy/free
  space is computed by unsigned 32-bit subtraction: `used = tail − head` (mod
  2³²). Because occupancy is bounded by `RING_SLOTS` (4096 ≪ 2³¹), the subtraction
  is unambiguous at every point of the wrap — the classic Lamport ring arithmetic.
- Full: `used ≥ RING_SLOTS − reserve` (reserve per class, §17). Empty: `used == 0`.
- **Why u32 wrap indices rather than (a) masked 32-bit indices with generation
  bits or (b) 64-bit indices:** (a) generation bits double the index state and add
  a class of ABA-style bugs for zero benefit given the occupancy bound; (b) u64
  indices require `BigInt64Array` atomics on the JS side — measurably heavier
  paths and a BigInt tax on every hot operation — while the wraparound argument
  above makes 64-bit width unnecessary for the ring (the 64-bit guarantee lives
  in `req_id`, §8.1, where it costs nothing because ids are never atomically
  RMW'd). [INFERRED from JS engine behavior; u32 idx verified by E5's u32 seq
  words at p50 0.01 ms.]

### 10.2 One submit ring per endpoint, single kernel consumer — justification

- **Single producer per submit ring:** worker-per-process (ADR-0003) means
  exactly one JS thread (the bridge) issues requests per process; `worker_threads`
  (Phase 3) will get one endpoint per thread — the SPSC invariant survives because
  the *endpoint*, not the process, owns the ring. SPSC needs no producer-side
  locking, and the memory-ordering proofs (§15) stay two-agent.
- **Single consumer (the one kernel worker):** Phase 2 has no kernel-side threads
  (directive; §22) — one kernel worker instance consumes every submit ring. No
  consumer-side synchronization exists at all in Phase 2.
- **One completion ring per endpoint:** the kernel is the single producer; the
  endpoint's worker the single consumer.

### 10.3 Sync priority lane (same ring, prioritized dequeue)

`MODE_SYNC` requests are enqueued on the **same** submit ring as async requests
(one submission path, one record format), but the kernel dequeues them with
priority:

- The producer increments `sr_sync_pending` (SeqCst add) when enqueuing a
  `MODE_SYNC` request; the kernel decrements it when it dequeues one.
- Each kernel pass processes endpoints in round-robin; per endpoint it first
  checks `sr_sync_pending` (one load). If > 0, it scans the unconsumed slot range
  (`sr_head..sr_tail`, bounded by occupancy — at most 4096 slot loads, only while
  a sync request is actually pending, i.e. ≤ 1 per sync call) and dequeues every
  `MODE_SYNC` entry it finds **before** ordinary FIFO dequeue of async entries.
- Skipping ahead is legal for the index contract: `sr_head` is only advanced over
  a *contiguous consumed prefix*; skipped-over async slots remain "in use" and
  still count toward the producer's full-check — backpressure stays correct.
- Starvation is impossible: at most one `MODE_SYNC` request is outstanding per
  endpoint (§11.1), so the priority dequeue is O(1) amortized per sync call.
- Alternative considered and rejected: a separate dedicated sync request slot. It
  would move sync requests off the single submission path — two submission
  channels, the very "two APIs" smell this ABI forbids — to save a bounded scan
  that happens at most once per sync call. [DESIGN-PROPOSAL]

### 10.4 Doorbells

| Doorbell | Word | Who bumps | Who waits | Wait primitive |
|---|---|---|---|---|
| Kernel doorbell | u32 @ `0x2000` (KSM-absolute) | every producer: `Atomics.add(1)` then `Atomics.notify(1)` after a submit | kernel worker | `Atomics.wait(word, expected, timeout)` (§10.5) |
| Completion doorbell | `cr_seq` (u32, per endpoint) | kernel: increment store + `notify(1)` after each `cr_tail` store | endpoint's bridge | `Atomics.waitAsync(word, expected)` (persistent, re-armed; §12.2) |
| Sync doorbell | `sm_seq` (u32, per endpoint) | kernel: increment store + `notify(1)` on sync completion / abandon-reset | endpoint's bridge (while blocked in a sync call) | `Atomics.wait(word, expected, rung_timeout)` (§11.3) |

A doorbell is **a wake hint, never a truth source**: the rings and mailboxes are
the truth; every waiter re-scans its ring/mailbox after every wake. The
no-lost-wakeup argument (waiter loads `expected` **before** scanning, then waits)
is proved in §15.5.

### 10.5 Kernel wait discipline (the kernel worker's event loop)

The kernel is driven by its worker's JS loop; the wasm kernel export
`nodine_kernel_pump()` is **non-blocking and never waits** (no
`memory.atomic.wait` inside kernel code — this keeps the kernel worker's JS
thread free to service the control plane and the SW bridge between passes, §13).
The loop:

```
loop forever:
    epoch = SeqCstLoad(kernel_doorbell)          # BEFORE scanning (§15.5)
    drained = kernel.pump()                      # wasm: dequeue/execute/complete
                                                 #   everything currently visible;
                                                 #   returns when all lanes are idle
    if kernel_state == STOPPED: exit loop
    timeout = min(250 ms, next_kernel_timer_deadline)   # heartbeats, watchers
    Atomics.waitAsync(doorbell, epoch, timeout)  # ADR-0011 D4: async wait keeps
                                                 #   the driver's event loop live
                                                 #   for the postMessage control
                                                 #   plane; same epoch discipline
    # woken by: any producer notify | timeout (tick timers, update heartbeat)
```

ADR-0011 (2026-11-10) — reference-driver notify routing (informational; the masks
are kernel-heap-internal, never bridge-visible): `nodine_pump` returns the
ASYNC-only notify mask; a second per-pass sync-activity mask (KH word, exported
via `nodine_sync_mask()`) marks endpoints that completed MODE_SYNC work. After
each pass the driver notifies `sm_seq[i]` for sync-mask bits and `cr_seq[i]` for
async-mask bits — a sync completion never spurious-wakes an async waiter chain.
The §16.5 shutdown sweep (both words, every endpoint) is unchanged. Bridges'
expected-value waits tolerate any wake pattern, so this routing is a latency
contract of the reference driver, not a bridge-visible semantic.

Measured anchors: this is exactly E5 phase a's `wait` mode (kernel blocks on the
request word) — p50 0.01 ms round trip (M1); the 250 ms ceiling bounds timer
coarseness and heartbeat updates without introducing the 24.59 ms-class tail of
the 1 ms-timeout hybrid (M4), because the timeout only fires when there is
genuinely no work (it never delays a notify — `Atomics.notify` wakes a waiting
agent directly; BROWSER_EXECUTION §4 Q2). Busy-spin (M3) is rejected: it
monopolizes a CPU on small-core machines. A `setTimeout(0)`-yield loop was
measured at sync RTT p50 4.4 ms (400× the §23 budget) and rejected (ADR-0011).

### 10.6 Capacity sizing rationale

`RING_SLOTS = 4096` per direction per endpoint (512 KiB slot storage per
endpoint, lazily committed — M11): package-manager workloads issue thousands of
concurrent `fs.promises` operations (`Promise.all` over store links —
`docs/REPORT_PHASE1.md` P1-E context); 4096 outstanding async requests per
process plus the bridge-side bounded queue (§17.3) covers pnpm-scale bursts
without kernel-side queueing. Below that, backpressure is explicit (§17). The
capacity is an ABI constant: changing it is a version bump (§20.3).

---

## 11. Endpoint class 1 — synchronous request/reply protocol

Serves every op in §7 in `MODE_SYNC`. The runtime worker submits, then blocks in
`Atomics.wait` on its sync mailbox until the kernel delivers the actual result
(the *result bytes are readable before the worker returns* — the Node-observable
contract of `*Sync`).

### 11.1 Single outstanding sync request per endpoint

A process worker has one JS thread; a sync Node call occupies it until it
returns; therefore at most one `MODE_SYNC` request per endpoint can be
outstanding at any instant. This invariant is load-bearing (sync-result
single-occupancy §9.4, priority-lane bound §10.3) and is checked by the bridge
(its sync submit path is not reentrant; Node JS cannot reenter it).

### 11.2 Submission sequence (producer side)

```
1. c          = SeqCstLoad(sm_seq)          # BEFORE submit (§15.5 no-lost-wakeup)
2. (ensure sm_state == IDLE, else §11.5)    # single-outstanding ⇒ normally true
3. CAS(sm_state, IDLE → SUBMITTED)          # SeqCst CAS — BEFORE ring publish
   # v1.0.1 (adversarial F-B2): the kernel may observe a ring entry only after
   # the mailbox is SUBMITTED. Publishing the ring first opened a window where
   # the kernel's complete-CAS found IDLE and discarded a result the worker was
   # about to wait on (lost completion + wedged mailbox). Order is normative.
4. write RequestRecord into submit-ring slot (idx = sr_tail)
   — plain stores; req_id = ++per-endpoint counter; FLAG_MODE_SYNC set
5. write payload into TX arena (sync reserve) — plain stores
6. SeqCstStore(sr_tail, idx + 1)            # publish the request
7. Atomics.add(sr_sync_pending, +1)
8. Atomics.add(kernel_doorbell, +1); Atomics.notify(kernel_doorbell, 1)
9. wait ladder on sm_seq (§11.3)

Pre-publish error rule (v1.0.1): if ANY step 3–5 fails locally (protocol
violation detected by the producer), the worker restores IDLE itself
(`CAS(SUBMITTED → IDLE)`) and surfaces the error — safe because the ring entry
was never published, so the kernel provably never observed the request. After
the ring publish (step 6+), only the §11.5 abandon protocol applies.
```

Kernel side (within its pump, per endpoint):

```
1. if SeqCstLoad(sr_sync_pending) > 0:
2.     scan slots sr_head..sr_tail for MODE_SYNC entries (§10.3)
3.     validate (§19.2), execute against VFS/process state
4.     write result: INLINE into the mailbox, or into the sync-result area
       (always empty — §9.4); plain stores
5.     SeqCstStore(sm fields, sm_req_id, sm_status, sm_kind, sm_off, sm_len)
6.     CAS(sm_state, SUBMITTED → COMPLETED)      # if it fails: abandoned — §11.5
7.     SeqCstStore(sm_seq, sm_seq + 1)            # actually: increment store
8.     Atomics.notify(sm_seq, 1)
9.     advance sr_head past the slot (SeqCstStore), decrement sr_sync_pending
```

### 11.3 Sync wait — timeout ladder

The worker blocks on the sync doorbell with an escalating rung ladder. Each rung
re-checks kernel health before re-arming (so kernel death is detected within one
rung, not at total timeout):

```
rungs (ms): 1, 5, 20, 100, 500, 500, 500, … (repeat 500 ms)
total budget: SYNC_TOTAL_TIMEOUT_MS = 10_000
per rung:
    r = Atomics.wait(sm_seq_view, 0, c, rung)
    if kernel_heartbeat_ms age > KERNEL_DEATH_MS (1000): → abandon, §11.5
       surfaced error: ERR_NODINE_KERNEL_DEAD
    if kernel_state ∈ {DRAINING, STOPPED}: → abandon; ERR_NODINE_KERNEL_SHUTDOWN
    if elapsed ≥ SYNC_TOTAL_TIMEOUT_MS:     → abandon; ERR_NODINE_KERNEL_TIMEOUT
    if r == "ok" or "not-equal": re-check sm_state (§11.4) — usually COMPLETED
```

Ladder justification: rung 1 ms resolves the overwhelmingly common case
immediately (M1: p50 0.01 ms, p99 0.025 ms — the first rung covers p99.999 by
distribution shape [INFERRED]); escalation avoids burning wakeups on a genuinely
busy kernel; the health checks convert a hang into a diagnosable error. Real Node
can block indefinitely on a wedged filesystem (NFS/FUSE precedent); we choose
explicit failure over silence — a deliberate, documented divergence.

Per-op execution budget (v1.0.1, adversarial F-B4 — heartbeat starvation):
every v1.0 op in the sync class is VFS/process-table-only (no external I/O,
no OPFS, no network) and MUST complete in < 10 ms p99 on the serialized kernel
— satisfied by construction (the costliest legal sync op is a bounded VFS
traversal or an ≤ 8128 B arena copy). Ops that could exceed this bound
(bulk/batch/fused/persistent) are async-class or chunked by definition.
`KERNEL_DEATH_MS` = 1000 ms therefore carries a ≥ 100× margin over any legal
sync op; a false `ERR_NODINE_KERNEL_DEAD` from heartbeat starvation is
impossible for conforming kernels, and a kernel blocking a sync op past the
budget is a kernel bug (P2-004 includes a T3-style regression test).

### 11.4 Wake and retire (consumer side)

```
1. CAS-free read: if sm_state == COMPLETED:
2.     read sm_status; if error → construct Node error (§14.3), CAS(sm_state, COMPLETED→IDLE), throw
3.     if sm_kind == INLINE: result = sm_inline bytes
       if sm_kind == SYNCRES: result = copy sm_len bytes from sync-result area
                              (a Float64Array view over the area at sm_off works — 8-aligned)
4.     CAS(sm_state, COMPLETED → IDLE)     # release; proves result consumed (§9.4)
5.     return the actual result to the Node binding call
```

The IDLE transition is the retire point for the sync-result area and the
mailbox; the next sync submission may reuse both.

### 11.5 Timeout / abandon protocol (no stale completions)

State machine on `sm_state` (all transitions SeqCst CAS; exactly one of
worker/kernel wins any race):

```
            submit                    complete                 retire
  IDLE ────────────────▶ SUBMITTED ────────────────▶ COMPLETED ────────────▶ IDLE
                           │   ▲                        │
              abandon CAS  │   │ discard + reset CAS    │ (not taken: retire is
                           ▼   │                        │  the only exit)
                        ABANDONED ◀─────┐
                           │            │ kernel sees ABANDONED, discards result,
                           └────────────┘ CAS(ABANDONED→IDLE), increments sm_seq,
                                          notifies (wakes the abandoning worker)
```

- Worker timeout path: `CAS(SUBMITTED → ABANDONED)`. If the CAS fails, the kernel
  completed concurrently → take the normal §11.4 path (a *late* result is still a
  correct result). If it succeeds, the worker waits (bounded: one more ladder,
  budget 1000 ms) for the kernel's `ABANDONED → IDLE` reset + notify, then
  surfaces the timeout/death error. The kernel's complete path does
  `CAS(SUBMITTED → COMPLETED)`; **complete-CAS failure handling covers every
  residual state (v1.0.1, adversarial F-B2)**:
  - state `ABANDONED` (worker timed out first): discard the result, reset
    `CAS(ABANDONED → IDLE)`, bump `sm_seq`, notify;
  - state `COMPLETED` (re-observe; structurally impossible on the serialized
    kernel): do nothing, never re-write fields;
  - state `IDLE` with `sm_req_id` ≠ this slot's `req_id`: the mailbox belongs
    to a different request — drop the result, advance the ring head, count a
    protocol diagnostic (§19.5). Unreachable for conforming producers since
    the v1.0.1 ordering (step 3 precedes publish); the kernel treats it as a
    bridge bug, never a silent discard.
- **No stale completions:** every sync request terminates in exactly one of
  {COMPLETED observed by worker, discarded on ABANDONED}; the mailbox never
  carries a completion for a request the worker did not issue (req_id checked),
  and an abandoned request can never later overwrite a subsequent request's
  completion — the kernel writes completion fields **before** winning
  `CAS(SUBMITTED → COMPLETED)` (the SeqCst CAS publishes those prior plain
  stores — §15.6; v1.0.1 correction of this sentence, which wrongly said
  "after" — the ordering is what makes the retire-side acquire read safe), and
  the worker writes a new request only after winning `CAS(IDLE → SUBMITTED)`;
  the two CAS domains are disjoint by state.
- If the kernel never performs the reset (kernel death), the worker's
  `KERNEL_DEATH_MS` heartbeat check fires and the bridge enters the
  kernel-dead state (§21.3) — the wait is bounded by construction.

### 11.6 Bridge-level chunking for oversized user payloads

All sync-mode results are bounded by `SYNCRES_SIZE − 64` (256 KiB − 64) and sync
payloads by **`TX_SYNC_RESERVE − 64` = 8128 B** (v1.0.1, adversarial F-B3: the
v1.0 text allowed sync payloads up to `TX_SIZE − 64`, which §17.1's 8 KiB
reserve cannot guarantee — 8 KiB–256 KiB sync writes were unserviceable under
async TX pressure while §17.1 called exhaustion "structurally impossible"). When a Node-level
operation would exceed a bound, the bridge decomposes it into multiple ABI calls:
`readFileSync` of a 5 MiB file → `OP_OPEN` + `OP_FSTAT` + 20 × `OP_READ(len ≤
256 KiB)` + `OP_CLOSE`. This mirrors upstream Node's own internal looping
(VERIFIED: `lib/fs.js:660-676` loops 8 KiB reads for unknown sizes;
`tryReadSync` loops for known sizes) and preserves Node-observable semantics
(partial-read states are unobservable; the event loop never runs mid-`*Sync`
call). Write chunking is **mandatory and normative** for sync writes above 8128 B:
the bridge decomposes `writeFileSync` into sequential `OP_WRITE`s of ≤ 8128 B
(POSIX makes no atomicity promise for regular-file writes above `PIPE_BUF`;
the serialized kernel cannot interleave another producer's write mid-sequence,
so Node-observable semantics are preserved; chunk-granularity divergence for
huge concurrent writers is documented §22). The v1.1 fused one-shot codes
(§7.5, E's G8) absorb this overhead for `readFile`/`writeFile` shapes when
adopted.

### 11.7 What the kernel may NOT do on the sync path

The kernel must complete any dequeued sync request without: writing to the
completion ring (no space dependency), allocating RX space (none exists for
sync), waiting on any consumer action, or requiring the endpoint's event loop to
run. These negatives are the deadlock-freedom core (§16.2).

---

## 12. Endpoint class 2 — asynchronous request/completion protocol

Same op set, `FLAG_MODE_SYNC = 0`. The caller receives its `req_id` immediately;
the worker's event loop keeps running; completions arrive on the completion ring.

### 12.1 Submission sequence

```
1. write RequestRecord into submit-ring slot (idx = sr_tail); plain stores
2. write payload into TX arena (async region); plain stores
3. register req_id in the bridge's outstanding table (promise/callback context)
4. if async occupancy (sr_tail − sr_head) ≥ RING_SLOTS − SYNC_RING_SLOTS
   or TX space insufficient → backpressure (§17.3), do NOT block
5. SeqCstStore(sr_tail, idx + 1)
6. Atomics.add(kernel_doorbell, +1); Atomics.notify(kernel_doorbell, 1)
7. return req_id to the caller (a Promise/FSReqCallback context lives bridge-side)
```

### 12.2 Completion delivery and the persistent `waitAsync` waiter

The bridge arms exactly one persistent waiter per endpoint:

```
arm():
    expected = SeqCstLoad(cr_seq)        # BEFORE scanning (§15.5)
    drain()                              # retire everything currently visible
    if kernel_state ∈ {DRAINING, STOPPED} → §21 handling
    Atomics.waitAsync(cr_seq_view, 0, expected)   # non-blocking (M9)
      .then(re-arm after drain)

drain():  # called on every wake
    while cr_head ≠ SeqCstLoad(cr_tail):
        rec = completion slot at cr_head
        validate (req_id outstanding? op echo? kind consistent? §19)
        copy result payload (RX area → JS object) or read inline
        deliver: resolve the promise / schedule FSReqCallback.oncomplete
                 on the event loop (queueMicrotask/setImmediate-equivalent —
                 the PoC's platform-timer completions produced byte-identical
                 event ordering to real Node, REPORT_PHASE1 §1)
        SeqCstStore(cr_head, cr_head + 1)     # retire AFTER copying (§9.3)
```

The wake is `waitAsync` (allowed in every worker; verified to keep the event
loop alive — M9). There is no polling: the waiter wakes on the kernel's
`cr_seq` notify or an immediate `"not-equal"` return.

### 12.3 Cancellation (summary; full semantics §18)

`OP_CANCEL` rides the async lane like any request. Outcomes: the target request
is either (a) not yet dequeued → removed, one completion with `status =
UV_ECANCELED` is emitted in the target's ring-order position; or (b) already
dequeued/executed → the cancel is a no-op and the real completion arrives. The
bridge's outstanding table accepts exactly one completion per id (first wins;
duplicates are a kernel bug and are dropped with a protocol diagnostic,
§19.5).

### 12.4 Ordering guarantees (per endpoint)

1. **Async submission order = dequeue order** (strict FIFO per endpoint, §10.2).
2. **Async completion order = dequeue order** (the kernel writes completions in
   the order it finishes ops, which is dequeue order; all payloads were
   pre-checked to fit, so no completion is deferred past a later one).
   ⇒ **async completion order = async submission order**, per endpoint.
3. `MODE_SYNC` requests interleave arbitrarily (priority lane; out-of-band
   mailbox completion) — sync and async ops have **no mutual order guarantee**,
   exactly as in real Node, where sync calls do not pass through the thread pool
   at all. The Phase 1 PoC's FIFO-async + immediate-sync discipline produced
   byte-identical event ordering to real Node v26.13.0 (T7, REPORT_PHASE1 §1) —
   evidence that this deterministic refinement sits inside Node's observable
   envelope. [VERIFIED for the tested workloads]
4. Across endpoints/processes: no order guarantee (kernel round-robins).
5. Ring-order vs id-order: a completion never overtakes an earlier completion on
   the same ring (single producer, FIFO slots); `OP_CANCEL`'s own completion
   follows its own submission position.

### 12.5 What the kernel may NOT do on the async path

The kernel never blocks the completion of an async op on consumer action: if RX
space is short it defers *dequeuing* that op (stays in the submit ring — the
producer is not blocked either, its ring accounting already counts the slot) and
moves on to other endpoints; it never waits, spins, or retries in-place
(§16.2–16.3).

---

## 13. Service Worker async path (bridged, not a second ABI)

Constraint (P1-F, VERIFIED topology): the SW intercepts preview fetches and must
never `Atomics.wait`; its event loop must never block on kernel internals; it is
idle-terminable (~30 s) and stateless (`docs/NETWORKING.md` §3). Nevertheless
SW-originated operations (Phase 5 vTCP ingress; Phase 2: the minimal
module-serving set — `OP_INGRESS_READ` only, §7.6) are class-2 requests of
**this** ABI:

```
Service Worker                         KERNEL WORKER (JS driver)              KSM
─────────────                          ───────────────────────────            ───
postMessage on a MessageChannel  ────▶  SW-bridge task (per request):
port (control plane; allowed)          1. parse ingress envelope (below)
                                       2. write the RequestRecord + payload
                                          into the INGRESS ENDPOINT's submit
                                          ring (same record format; the driver
                                          is an in-agent producer — no atomics
                                          needed to see its own writes)
                                       3. Atomics.add/notify(kernel_doorbell)
                                       4. kernel.pump() (drains all lanes,
                                          incl. the ingress endpoint's)
                                       5. read the ingress endpoint's
                                          completion ring (in-agent consumer)
                                       6. postMessage the completion envelope
                                       ◀──── back over the MessageChannel
(promise resolution; streamed Response
 for fetch ingress — P1-F E1–E3 pattern)
```

- **Ingress endpoint:** an ordinary endpoint slot allocated by the kernel at
  boot (endpoint table `ep_caps = INGRESS|ASYNC`), owned by the kernel worker's
  JS driver. It has no runtime worker; the driver is both producer and consumer
  *proxy*. Because producer and consumer are the same agent, the driver may read
  its own ring without atomics; it still uses the identical record layouts and
  the identical kernel dequeue path — the request enters the kernel exactly like
  any other (the doorbell add makes the wake uniform; the immediate `pump()`
  makes latency independent of the doorbell).
- **Envelope (transport framing only):** `[u32 total_len][RequestRecord (64 B,
  verbatim)][payload bytes…][CompletionRecord (64 B, verbatim)][payload bytes…]`
  carried in a transferred `ArrayBuffer` over the MessageChannel. The records
  inside are byte-identical to the shared-memory records; the u32 length prefix
  is framing, not a schema. This is the *only* permitted non-KSM carriage of ABI
  records, it is confined to the SW bridge (control-plane class), and it is
  never used for runtime-worker traffic (hot path stays pure shared memory —
  directive §2.1/§8, ARCHITECTURE §4).
- **No Atomics in the SW. No KSM reference in the SW** (the SW holds no
  SharedArrayBuffer at all — nothing to lose across idle termination; state is
  re-fetched on wake per `docs/NETWORKING.md` §3). The SW waits only on
  MessageChannel promises, woken by the kernel worker's driver, which itself is
  never blocked (§10.5: the driver never waits inside wasm).
- **Why this is not a second ABI:** operation ids, request/completion record
  layouts, error codes, result kinds, and semantics are the same objects defined
  in §6–§8; the MessageChannel hop is a byte-preserving transport shim at one
  endpoint of the same interface. Adding SW-visible ops (vTCP, Phase 5) extends
  §7's op table through normal versioning (§20.3), not a parallel API.

### 13.5 Persistence channel (v1.0.1 — one home for the storage transport; P2-008)

The kernel↔persistence-worker transport is a **private, non-hot-path channel**,
not part of the per-endpoint ABI rings: journal/snapshot batches are async by
nature and never sit on the Node syscall path (directive §2.1 — this is a
kernel-internal control-plane channel in the postMessage-for-control-plane
class, not a second ABI). This section eliminates the v1.0 spec orphan
(adversarial G2b C5: a `PERSIST` capability bit was reserved with zero ops and
no owning document for the ring's memory home, record format, or wake
discipline):

- **Memory home (this document, §5.2):** the 64 KiB region at
  `0x0505_0000 – 0x0505_FFFF` (the v1.0 alignment pad), shared between the
  kernel worker and the persistence worker only.
- **Layout, record format, `durable_seq` word, and wake discipline (one
  owner):** `docs/phase2/OPFS_PERSISTENCE.md` §4 — that document is normative
  for the channel's contents; this document is normative for its existence,
  bounds, and memory home. The kernel wakes the persistence worker via
  `Atomics.notify` on the channel's doorbell word (the persistence worker is a
  Dedicated Worker — `Atomics.wait` is legal there); the persistence worker
  never touches any other KSM region.
- **Capability bit:** `KERNEL_CAP_PERSIST` (bit 3, §21 table) now means "the
  persistence channel is active"; no ops are added to the §7 op table for it
  in v1.0.1. `OP_FSYNC` (0x0120) is the ONLY fs op that interacts with the
  channel (drain + `flush` ack; a persistence stall past the §11.3 ladder ⇒
  `EIO`).

---

## 14. Error model and Node error mapping

### 14.1 Status field semantics (both completion lanes)

`status` is an `i32` in every completion (§6.2 `status`, §6.3 `sm_status`):

| Range | Namespace | Meaning |
|---|---|---|
| `0` | — | success |
| `-1 … -4095` | **libuv errno (Linux table)** | filesystem/process errors with a uv/POSIX analog — values are EXACTLY the vendored table (Appendix A; `poc/p1-poc/uv-errno-linux.txt`, generated from Node v26.13.0's uv headers) |
| `-0x1000_0000 … -0x1000_00FF` | **`NODINE_*`** | kernel-protocol / kernel-health errors with **no** uv analog |

**Principle:** the kernel returns a uv errno whenever one exists (including
`ENOMEM` for kernel-heap exhaustion, `EMFILE/ENFILE` for fd/table exhaustion,
`ECANCELED` (−125) for cancellations). `NODINE_*` codes exist only for
conditions real kernels never surface to `fs()` — protocol violations and
kernel-health events. There is no third namespace and no positive-status
encoding.

### 14.2 `NODINE_*` codes (Rust: §24.3)

| Code | Value | Meaning | Surfaced to Node as |
|---|---|---|---|
| `NODINE_EINVAL_REQUEST` | `-0x1000_0000` | malformed request: unknown flag bit, non-zero unused arg, bad arg encoding | `ERR_NODINE_PROTOCOL` (async; a bug in the bridge) — sync: same, thrown |
| `NODINE_ENOOP` | `-0x1000_0001` | op id not defined in this ABI version / stub op | `ERR_NODINE_PROTOCOL` (bridge mapped a Node call to a stub — a version-skew bug) or `ENOSYS` where the bridge deliberately probes |
| `NODINE_EABI_VERSION` | `-0x1000_0002` | probe/version mismatch | `ERR_NODINE_ABI_VERSION` (fatal at bring-up, §20) |
| `NODINE_ESHUTDOWN` | `-0x1000_0003` | request submitted while `kernel_state ∈ {DRAINING, STOPPED}` | `ERR_NODINE_KERNEL_SHUTDOWN` |
| `NODINE_ETIMEOUT` | `-0x1000_0004` | sync ladder exhausted (bridge-side; also used if the kernel ever enforces op deadlines) | `ERR_NODINE_KERNEL_TIMEOUT` |
| `NODINE_EKERNEL_DEAD` | `-0x1000_0005` | heartbeat lost (bridge-side diagnosis) | `ERR_NODINE_KERNEL_DEAD` |
| `NODINE_EENDPOINT` | `-0x1000_0006` | endpoint state/generation invalid | `ERR_NODINE_ENDPOINT_STATE` |
| `NODINE_EPROTOCOL` | `-0x1000_0007` | direction/kind/consistency violation (e.g. `sm_kind = RX_ARENA`) | `ERR_NODINE_PROTOCOL` |
| `NODINE_EBADCORRELATION` | `-0x1000_0008` | completion for an id the kernel does not owe (internal invariant; defensive) | `ERR_NODINE_PROTOCOL` |

These are Nodine-internal error **codes** surfaced as Node `Error` objects with
`code: 'ERR_NODINE_*'` (upper-case, Node internal-code style). They are never
produced by a healthy system and always indicate bridge/kernel bugs or kernel
death — which is why they do not participate in the uv mapping.

### 14.3 Node error construction — the bridge owns it

The kernel returns **numbers only**. The bridge constructs Node-conformant error
objects from its own call context (it knows `syscall`, `path`, `dest` — it
submitted them), exactly as the Phase 1 PoC does (VERIFIED `poc/p1-poc/worker.mjs`
`uvErr`/`uvThrow`/`fillCtx` at 1519-1523, 1851-1864; upstream shape:
`lib/internal/errors.js` `uvException`):

- **Sync path (`*Sync` calls):** throw an `Error` with:
  `err.errno = <negative uv errno>` · `err.code = '<UVNAME>'` (e.g. `'ENOENT'`) ·
  `err.syscall = '<syscall>'` · `err.path` / `err.dest` when applicable · message
  text `"<CODE>: <uv message>, <syscall> '<path>'"` — byte-shape identical to
  Node's `UVException` rendering (the PoC's messages passed the Phase 1 gate
  tests; exact message strings re-verified against `lib/errors.js` in P2-006).
- **Async path:** pass the same-shaped `Error` object to
  `FSReqCallback.oncomplete(err)` (or reject the `fs.promises` promise with it).
  VERIFIED protocol: `worker.mjs:1651-1706` (`FSReqCallback`, `ReadFileJob`,
  `WriteFileJob` complete with `req.oncomplete.call(req, err)` / `(null, …)`).
- **`EISDIR` on read:** Node surfaces `ERR_FS_EISDIR` (`code: 'EISDIR'`) for
  read-on-directory in several APIs (`lib/internal/fs/utils.js:1037-1044` —
  VERIFIED); the bridge maps kernel `EISDIR` (−21) accordingly per call site.
- **`throwIfNoEntry`** (`stat` family): the kernel always returns `ENOENT`; the
  bridge suppresses the throw and returns `undefined` where Node's option says so
  (VERIFIED `worker.mjs:1798-1808`).

### 14.4 Mapping table (uv errno → Node shape), common paths

| Kernel status | Code | Typical ops | Node sync surface | Node async surface |
|---|---|---|---|---|
| `0` | — | all | return value | `oncomplete(null, …)` / promise resolve |
| `-2` | `ENOENT` | OPEN, STAT, UNLINK, RENAME, RMDIR, MKDIR(parent) | throw `{errno:-2, code:'ENOENT', syscall, path}` | `oncomplete(err)` |
| `-17` | `EEXIST` | OPEN(O_EXCL), MKDIR | throw `EEXIST` | `oncomplete(err)` |
| `-20` | `ENOTDIR` | OPEN, MKDIR, RENAME | throw `ENOTDIR` | `oncomplete(err)` |
| `-21` | `EISDIR` | OPEN(write on dir), READ(dir fd) | throw `EISDIR` (read: `ERR_FS_EISDIR` shape at call sites per §14.3) | `oncomplete(err)` |
| `-9` | `EBADF` | CLOSE/READ/WRITE/FSTAT on bad fd | throw `EBADF` | `oncomplete(err)` |
| `-13` | `EACCES` | OPEN, MKDIR, UNLINK… | throw `EACCES` | `oncomplete(err)` |
| `-22` | `EINVAL` | bad flags/args (post-validation) | throw `EINVAL` | `oncomplete(err)` |
| `-36` | `ENAMETOOLONG` | path > 4096 (bridge pre-check + kernel check) | throw `ENAMETOOLONG` | `oncomplete(err)` |
| `-39` | `ENOTEMPTY` | RMDIR | throw `ENOTEMPTY` | `oncomplete(err)` |
| `-11` | `EAGAIN` | **async submit backpressure** (§17.3) | (never sync — reserve) | `oncomplete({errno:-11, code:'EAGAIN', syscall})` — documented divergence, §17.4 |
| `-125` | `ECANCELED` | CANCEL target | — | `oncomplete(ECANCELED)` for the canceled request |
| `-12` | `ENOMEM` | kernel heap exhausted | throw `ENOMEM` | `oncomplete(err)` |
| `-24`/`-23` | `EMFILE`/`ENFILE` | fd table / process table full | throw `EMFILE`/`ENFILE` | `oncomplete(err)` |
| `-4094` | `UNKNOWN` | unmapped internal failure | throw `UNKNOWN` | `oncomplete(err)` |

The full uv table (all 74 names/values) is Appendix A; the kernel may return any
of them; the bridge maps by table lookup (PoC `codeForUv`, `worker.mjs:1861-1864`
— VERIFIED mechanism).

### 14.5 `EAGAIN` semantics vs real Node (honest divergence)

Real Node queues unbounded async fs requests (libuv thread-pool queue) and never
returns `EAGAIN` from fs promises. Nodine bounds every queue (directive §8:
"No unbounded queues. Backpressure is mandatory"): per endpoint ≤ 4096 in-ring +
≤ `BRIDGE_QUEUE_MAX` (default 65536, §17.3) bridge-queued async ops. When both
are exhausted, `fs.promises`/callback APIs surface `EAGAIN` rejections. This is a
documented, deliberate divergence with three mitigations: (1) the bounds are
configurable at boot; (2) 4096+65536 outstanding un-retired fs ops is far beyond
observed package-manager behavior (P1-E); (3) the bridge queue drains
event-driven (on retire), so the bound is hit only by a consumer that stops
retiring entirely — the same workload that would OOM real Node. [DESIGN-PROPOSAL
grounded in directive §8]

---

## 15. Memory ordering and happens-before proofs

### 15.1 The three memory models, mapped

| Plane | Spec anchor [DOCUMENTED] | Ordering discipline used here |
|---|---|---|
| JS (bridge, kernel driver, host) | ECMA-262 **Atomics**: every `Atomics.load/store/add/compareExchange` on a `SharedArrayBuffer` view is a **sequentially consistent** atomic; `Atomics.wait/notify` are the agent-cluster blocking primitives; the ECMAScript **SharedArrayBuffer memory model** (agent clusters, shared data blocks) defines cross-agent visibility | All JS-side shared-memory accesses use `Atomics.*` (SeqCst by spec) on `Int32Array`/`Float64Array`/`BigInt64Array` views; plain (non-atomic) accesses occur **only** on payload bytes whose visibility is established by the index/doorbell atomics (Lemmas L1–L3) |
| Rust kernel/bridge (wasm32) | **WebAssembly threads extension** (WebAssembly 2.0 Specification, "Threads" / atomic instructions): atomic accesses carry explicit orderings (`seq_cst`, `acq_rel`, `acquire`, `release`); **unaligned atomic access traps**; non-atomic loads/stores are ordered by the wasm memory model relative to atomics in the same agent | All Rust shared state is `core::sync::atomic::AtomicU32/AtomicU64` with **`Ordering::SeqCst`** uniformly; payload copies use plain `ptr` reads/writes guarded by the same index atomics |
| Interop | JS SeqCst atomics and wasm `seq_cst` atomics on the **same** shared memory compose into one sequentially consistent history per the two specs' shared-memory semantics (both build on the same underlying futex/atomic primitives of the engine; E3 VERIFIED cross-agent atomics on shared wasm memory, M7) | One uniform SC discipline across all agents |

**Decision — uniform SeqCst (both languages, every shared word).** Rationale: (1)
the JS side *cannot* order-weaken (spec mandates SC), so Rust-side
Acquire/Release mixing would create an asymmetric proof obligation for zero
measurable gain; (2) every proof below then reduces to "there is a single total
order S of all atomic accesses; program order + reads-from closure yields
happens-before" — mechanically checkable; (3) cost: the measured 0.01 ms round
trip (M1) *already includes* four SC atomics + two futex ops; an SC store vs
Release store difference is single-digit ns — 3 orders of magnitude below the
transport budget (§23). Relaxations may be introduced later **only** with
P2-009 measurements and re-proof — a performance change, not a correctness
fallback. [DESIGN-PROPOSAL]

### 15.2 Notation

`→po` program order (same agent) · `→rf` reads-from · `→S` the total order of all
SeqCst atomics on the KSM (exists per §15.1) · `hb = (→po ∪ →rf_sc)⁺` where
`→rf_sc` links an SC store to the SC load that reads it. For payload (non-atomic)
bytes, we prove `write →hb read` via the guard atomics.

### 15.3 Payload lemmas

**L1 (request submission → kernel observes request).** Producer: plain stores of
the `RequestRecord` + TX payload (`→po`) precede `SeqCstStore(sr_tail, t+1)`
(`→po`), which precedes `Atomics.add(kernel_doorbell)` + `notify`
(`→po`). Kernel: on wake (or timeout), `SeqCstLoad(sr_tail)` reads `t+1` (or
later) ⇒ `store_tail →S load_tail`. Then `SeqCstLoad(sr_head)` and the plain
reads of slot/payload bytes follow (`→po`). By SC total order + program order:
`slot-writes →po store_tail →S load_tail →po slot-reads` ⇒ `slot-writes →hb
slot-reads`. The kernel reads fully-initialized request bytes. ∎

**L2 (kernel writes result → Node worker observes and retires).** Kernel: plain
stores of result bytes (RX area / sync-result area / mailbox fields) (`→po`)
precede `SeqCstStore(cr_tail, c+1)` / `SeqCstStore(sm_seq, s+1)` (`→po`), which
precede `Atomics.notify` (`→po`). Consumer: `Atomics.wait`/`waitAsync` returns
after the kernel's `notify` (wait/notify are the agent-cluster synchronization
primitives — the ECMAScript spec requires the waiter's continuation to observe
effects sequenced before the notify in S), then `SeqCstLoad(cr_tail)`/state check
reads the new tail (`→rf`), then plain reads of the completion + payload follow
(`→po`). Chain: `result-writes →po store_tail →S load_tail →po result-reads` ⇒
`result-writes →hb result-reads`. ∎

**L3 (retire → kernel reuse is safe).** Consumer: plain reads of the payload
(`→po`) precede `SeqCstStore(cr_head, h+1)` / CAS `COMPLETED → IDLE` (`→po`).
Kernel: before reusing retired RX-arena space or the sync-result area,
`SeqCstLoad(cr_head)` / CAS-load of `sm_state` reads the retire store (`→rf`),
then reuse-writes follow (`→po`). Chain: `payload-reads →po retire-store →S
kernel-load →po reuse-writes` ⇒ the consumer's reads `→hb` the kernel's
overwrite; no torn reads. ∎

**L4 (TX reuse is safe).** Producer reuses TX bytes only after observing the
request's completion (retire of that request, or sync IDLE transition) — by
§9.2's lifetime rule. Kernel reads of those bytes were sequenced before its
completion store (L1/L2 machinery, `→po` inside the kernel) ⇒ kernel reads `→hb`
producer reuse-writes. ∎

**L5 (ring-slot reuse).** Same shape as L3/L4 with `sr_head`/`sr_tail`: a slot is
rewritten by the producer only when `sr_tail − sr_head` has room, which requires
reading a `sr_head` that the kernel stored only after its plain reads of the slot
(L1); a completion slot is rewritten by the kernel only when `cr_tail − cr_head`
has room, which requires reading a `cr_head` stored only after the consumer's
plain reads (L2). ∎

### 15.4 Field-by-field concurrency contract

| Field (§5/§6 offsets) | Writer → op | Reader → op | Guarantee |
|---|---|---|---|
| KCTL `magic/abi_version/consts` | kernel, SeqCst store, once, before `kernel_state=RUNNING` | all, SeqCst load **after** loading `kernel_state` | L2-pattern; stable thereafter |
| KCTL `generation` | kernel, SeqCst store, per boot, before state | bridge, SeqCst load at wake points | change ⇒ kernel rebooted (§21.3) |
| KCTL `kernel_state` | kernel, SeqCst store | all, SeqCst load (waiter rungs, drain loop) | monotonic BOOTING→RUNNING→DRAINING→STOPPED |
| KCTL `kernel_heartbeat_ms` | kernel, SeqCst store ≥ every 250 ms | bridge/host, SeqCst load | advisory; staleness = liveness signal only |
| `kernel_doorbell` (u32 @0x2000) | producer `Atomics.add(1)`; kernel never writes | kernel `Atomics.wait(word, epoch, timeout)` + SeqCst load | epoch counter; §15.5 |
| Endpoint-table slot | kernel, SeqCst stores, `state` last | bridge, SeqCst loads, `state` first | discovery; immutable while live |
| `sr_tail` | producer SeqCst store | kernel SeqCst load | L1 |
| `sr_head` | kernel SeqCst store | producer SeqCst load | L5 (full-check) |
| `sr_sync_pending` | producer `Atomics.add`; kernel `Atomics.add` (dequeue) | both SeqCst load | hint; correctness never depends on it (kernel scans when in doubt) |
| submit-slot + TX bytes | producer plain writes | kernel plain reads | L1/L4 |
| `cr_tail` | kernel SeqCst store | consumer SeqCst load | L2 |
| `cr_seq` (doorbell) | kernel SeqCst store (increment) + `notify(1)` | consumer `Atomics.waitAsync(word, expected)` | §15.5 |
| `cr_head` | consumer SeqCst store | kernel SeqCst load | L3/L5 |
| completion-slot + RX bytes | kernel plain writes | consumer plain reads | L2/L3 |
| `sm_seq` (doorbell) | kernel SeqCst store (increment) + `notify(1)` | worker `Atomics.wait(word, expected, rung)` | §15.5 |
| `sm_state` | both, **SeqCst CAS only** (§11.5 transitions) | both, SeqCst load/CAS | CAS-linearized state machine; exactly one winner per race |
| mailbox completion fields + sync-result bytes | kernel plain writes | worker plain reads | L2 (guarded by `sm_seq`/`sm_state=COMPLETED`) |
| `ep_heartbeat_ms` | bridge SeqCst store | kernel SeqCst load | advisory liveness |
| `ep_state` | kernel SeqCst store | bridge/host SeqCst load | lifecycle |

### 15.5 No-lost-wakeup lemma (doorbell discipline)

Pattern (used by kernel doorbell, `cr_seq`, `sm_seq`): waiter does
`e := SeqCstLoad(doorbell); scan truth (rings/mailbox); if no work:
Atomics.wait(doorbell, e, timeout)`. Poster does
`publish-truth (SeqCst store of tail/seq); Atomics.add/notify(doorbell)`.

Claim: the waiter never sleeps past published work. Proof sketch (all atomics
SC, one total order S; poster's truth-publish `→po` its doorbell bump): suppose
the waiter's scan observed no work. Case (a): the poster's truth-store precedes
the waiter's truth-scan in S — then the scan would have seen the work
(contradiction, by L1/L2 visibility). Case (b): the poster's truth-store follows
the waiter's scan in S. Then the poster's doorbell bump (which follows its
truth-store by `→po`) also follows the scan. If the bump precedes the waiter's
`wait` entry-check in S, the check reads a value ≠ `e` ⇒ wait returns
`"not-equal"` immediately ⇒ waiter re-scans and finds the work. If the bump
follows the wait entry, the waiter is inside the futex when the `notify` lands ⇒
`wait` returns `"ok"`. Either way the waiter re-scans. `waitAsync` behaves
identically (its promise resolves on notify or an immediate not-equal). ∎
(This is the discipline E5 used — `poc/p1-c/public/e5-client.js:27` waits with
the prior sequence value, `e5-kernel.js:33` waits with `last`; 1000/1000
iterations ok in both phases a and b — VERIFIED.)

### 15.6 CAS state machine (sync mailbox) ordering

All `sm_state` transitions are SeqCst CAS. The critical race — worker abandons
(`SUBMITTED→ABANDONED`) exactly as the kernel completes
(`SUBMITTED→COMPLETED`) — is decided by the CAS linearization point: exactly one
CAS succeeds; the loser observes the new state and follows its branch (§11.5).
Because the kernel writes completion *fields* **before** winning its CAS (the
SeqCst CAS publishes those plain stores — release; v1.0.1 correction, this
sentence previously and wrongly said "after", contradicting §11.2's pseudocode
and empirically delivering a stale result on the abandon path — torture T2),
and the worker reads them only after observing `COMPLETED` (or its own CAS
failure), the field visibility is L2-shaped. ∎

---

## 16. Deadlock-freedom analysis

Directive §8: "Deadlocks are tested", "Never wait while holding a lock required
by the kernel worker", "Ensure shutdown and cancellation can wake waiting
workers". This section is the design-level proof; P2-004 tests it.

### 16.1 Complete wait-edge enumeration

| # | Waiting agent | Waits on | Wake sources | Bounded? |
|---|---|---|---|---|
| W1 | Runtime worker (sync call) | `sm_seq` via `Atomics.wait` rung ladder (§11.3) | kernel sync completion / abandon-reset notify; rung timeout | yes — rung ≤ 500 ms, health checks each rung, total 10 s |
| W2 | Kernel worker (idle) | `kernel_doorbell` via `Atomics.wait(epoch, timeout)` (§10.5) | any producer's `add+notify`; timeout ≤ 250 ms | yes — timeout, by construction |
| W3 | Bridge async waiter | `cr_seq` via `Atomics.waitAsync` | kernel notify / not-equal | **non-blocking by definition** (M9) |
| W4 | Service Worker | MessageChannel promises (no Atomics) | kernel worker's driver `postMessage` | non-blocking (event-loop based) |
| W5 | Host (main thread) | nothing in the ABI (never waits on Atomics — M6) | — | — |

Every `Atomics.notify` in the system is issued **without the poster waiting for
anything** (`notify` is a non-blocking primitive). There are no other blocking
primitives in the design: the kernel wasm code contains no `memory.atomic.wait`
(§10.5), no `postMessage` awaits on the hot path, and no kernel-internal locks
(single kernel agent — §22).

### 16.2 Sync-completion independence theorem

*While a process worker is blocked in W1, the kernel can always complete that
worker's sync request without any action from that worker.* The kernel's sync
path (§11.7 negatives) depends only on: (a) the submit ring having the request —
guaranteed by L1 and the sync slot reserve (§17.1: a sync submit never fails for
ring/arena space, and at most one is outstanding); (b) sync-result space — the
sync-result area is single-occupancy and always empty (§9.4); (c) the mailbox —
one fixed 64 B cell, always writable; (d) kernel liveness — W2 is woken by the
submitting worker's own doorbell bump, which by §11.2 step 7 **precedes** the
worker entering W1 (program order), so by §15.5 the kernel either sees the
request on its current pass or wakes for it. The kernel never dequeues other
work "under" the sync request in a way that requires the blocked worker: async
ops of the same endpoint that cannot complete (RX full) are *not dequeued*
(§12.5) — they wait in the ring, requiring nothing. ∎

### 16.3 Async deferral theorem

*The kernel never blocks (W2's timeout aside) on a consumer.* RX-arena
shortfall ⇒ defer dequeue of that endpoint's async ops (they stay in the ring);
completion-ring fullness cannot occur for sync (mailbox) and for async implies
the consumer is not retiring, in which case the kernel stops dequeueing that
endpoint and proceeds to other endpoints; TX payload space is the producer's own
resource. At no point does the kernel wait, poll, or retry — it defers and
returns to its pump loop. ∎

### 16.4 Acyclicity of the wait-for graph

The only *blocking* agents at any instant are: some set of process workers in
W1 (each waits only on its **own** kernel-written doorbell), the kernel in W2
(waits on any producer or its own timeout), and nobody else. Wait-for edges:
`worker_i → kernel` (W1) and `kernel → {any producer}` (W2). Every W2 edge is
satisfied by *non-blocking* notifies already issued (§11.2 step 7 ordering) or
by the timeout — W2 never creates a dependency on a *currently blocked* agent's
future action: a worker in W1 has already posted everything the kernel needs
from it (the request and the doorbell bump) before blocking. Hence there is no
path `kernel → worker_i → … → kernel`: edges from the kernel point only to
agents that either never block on the kernel (posters in W3/W4/W5) or have
already unblocked it (workers in W1 pre-notify). The wait-for graph is
therefore acyclic; no deadlock is constructible. ∎

### 16.5 Shutdown wakes all waiters

Graceful shutdown (§21.1): the kernel sets `kernel_state = DRAINING|STOPPED`
(SeqCst) and then notifies **every** endpoint's `cr_seq` and `sm_seq` (a fixed,
≤ `MAX_ENDPOINTS`×2 sweep of `Atomics.notify`). Every W1 waiter is inside
`Atomics.wait` on exactly `sm_seq` ⇒ wakes (notify or not-equal) ⇒ its rung
handler loads `kernel_state` ⇒ observes shutdown ⇒ abandons (§11.5) and errors
with `ERR_NODINE_KERNEL_SHUTDOWN`. Every W3 waiter's `waitAsync` promise
resolves ⇒ drain loop observes `kernel_state` ⇒ fails pending ops with
`NODINE_ESHUTDOWN` completions already written by the kernel for in-flight
requests (§21.1). The kernel worker itself exits W2 by its own state check. No
agent remains blocked on a stopped kernel. ∎

### 16.6 "No wait while holding a shared resource" audit

Directive §8: never wait while holding a lock the kernel needs. The kernel holds
**no cross-agent locks** (single consumer; kernel-internal data is
single-agent). Ring slots and arena bytes are "held" by producers across waits
(e.g. a W1 worker's request occupies its slot and TX bytes while it waits) — but
the kernel requires **nothing** from the producer to consume them (reads are
producer-independent, L1/L4). The sync mailbox is held by the worker only in
`IDLE→SUBMITTED`, which is exactly the state the kernel needs to proceed. The
CAS transitions are instantaneous (no wait between CAS and the guarded writes).
∎

---

## 17. Backpressure and queue-full policies

### 17.1 Sync class — reserves, not blocking

A sync submit can fail for space only if the producer violated the
single-outstanding invariant; the ABI makes space exhaustion **structurally
impossible** for a conforming bridge (payloads within the §11.6 cap; v1.0.1):

| Resource | Reserve rule | Constant |
|---|---|---|
| Submit-ring slots | async submissions must keep `sr_tail − sr_head ≤ RING_SLOTS − 64`; sync submissions may consume the last 64 slots | `SYNC_RING_SLOTS = 64` |
| TX arena bytes | async allocations must keep the last 8 KiB free; sync payloads (≤ 8128 B = `TX_SYNC_RESERVE − 64`, §11.6 — paths and chunk-sized writes; larger sync writes are mandatorily bridge-chunked) draw from the reserve | `TX_SYNC_RESERVE = 0x2000` |
| Result space | sync-result area (256 KiB) is single-occupancy and always empty (§9.4) | `SYNCRES_SIZE = 0x4_0000` |

If a sync submit nevertheless finds the mailbox non-IDLE or the reserve consumed
(a bridge bug), the bridge surfaces `NODINE_EPROTOCOL` diagnostics — the
kernel never blocks the sync producer. The task's allowance "for sync class the
producer may block-with-timeout" is **not needed and deliberately not used for
space**: the sync producer's only wait is the completion ladder (§11.3), which is
bounded and health-checked. This is strictly stronger than block-with-timeout.
[DESIGN-PROPOSAL]

### 17.2 Kernel-side deferral (async; never blocks, never errors)

When the kernel cannot write an async payload (RX free < need, §9.3) or the
completion ring is full, it defers dequeueing that endpoint (strict FIFO — later
ops of that endpoint wait too) and moves on. Observable effect: latency on that
endpoint only. This mirrors libuv's thread-pool queueing under load.

### 17.3 Async class — producer must NOT block; explicit EAGAIN

Producer-side checks at submit (§12.1 step 4), in order:

1. **Bridge queue** (bridge-local, bounded `BRIDGE_QUEUE_MAX = 65536` default):
   if the submit ring is full *and* the queue is full → the Node call fails
   immediately with **`EAGAIN`** (uv −11; async surfaces as promise rejection /
   `oncomplete(err)`, §14.4). The bridge queue exists because real workloads
   burst past 4096 concurrent ops (P1-E pnpm `Promise.all`); it is
   producer-local flow control (exactly libuv's own request queue in front of a
   bounded pool), not a second transport: queued requests are `RequestRecord`s
   awaiting a ring slot, submitted unchanged when a retire frees one
   (event-driven — each retire signals the queue).
2. **Ring full** (`sr_tail − sr_head > RING_SLOTS − SYNC_RING_SLOTS`) → enqueue
   into the bridge queue (no error yet).
3. **TX space** exhausted (async region) → enqueue into the bridge queue.
4. **Kernel-side authority:** if the kernel nonetheless finds an async request it
   cannot honor at dequeue time (stale reserves, deferral starvation beyond
   `ASYNC_DEFER_LIMIT` passes), it completes the request with `EAGAIN` — the
   authoritative error path (producer-side checks are advisory optimizations).

Justification vs Node's real behavior: §14.5. The asymmetry (sync never EAGAINs,
async can) matches the platforms: real sync syscalls either complete or error on
the caller's thread; real async queues are bounded by memory in practice.

### 17.4 Queue-full summary

| Condition | Sync | Async |
|---|---|---|
| submit ring full | impossible (reserve, §17.1) | bridge queue → then `EAGAIN` |
| TX arena full | impossible (reserve) | bridge queue → then `EAGAIN` |
| completion ring full | n/a (mailbox) | kernel defers dequeue (latency only) |
| RX arena full | n/a (sync-result area) | kernel defers dequeue (latency only) |
| kernel heap exhausted | `ENOMEM` completion | `ENOMEM` completion |
| fd table exhausted | `EMFILE` completion | `EMFILE` completion |

---

## 18. Cancellation

`OP_CANCEL` (`0x00F0`, async mode, `a0` = target `req_id`): explicit, best-effort,
race-tolerant.

- **Not yet dequeued** (kernel-side outstanding table lookup at dequeue pass):
  the kernel removes the target from the ring's unconsumed range (marks the slot
  consumed at its position — slot accounting stays FIFO-correct), and emits
  **one** completion with `status = UV_ECANCELED (−125)`, `req_id` = target,
  in the target's ring-order position.
- **Already dequeued or executing:** the cancel completes with `status = 0`
  ("no-op") for the cancel itself; the target completes normally. Phase 2 kernel
  ops are memory-VFS bound (µs–ms); there is no interruptible long-running op;
  the kernel *may* check the cancel table between op phases but is not required
  to (honesty: cancellation of executing work is not preemptive — §22).
- **Exactly-once invariant:** each request receives exactly one completion
  (normal or canceled); `OP_CANCEL` never causes a second. The kernel's
  outstanding-table removal is the linearization point.
- **No stale completions:** a canceled request's ECANCELED completion carries the
  request's own `req_id`; the bridge's outstanding table consumes it; any later
  duplicate (kernel bug) is dropped with `ERR_NODINE_PROTOCOL` diagnostics
  (§19.5).
- **In-flight kernel work:** for Phase 2 this is bounded and non-preemptible
  (§22); the mechanism (cancel marks in the kernel outstanding table) is the
  hook future long-running ops (watchers, Phase 5 sockets) will check at
  cooperative yield points.
- **Node mapping:** Node's fs API has no user-facing cancel; `OP_CANCEL` is used
  by the bridge itself during endpoint drain (§21.2) to fail pending async ops
  with `ECANCELED` (matching how real Node fails pending requests when a handle
  is destroyed). User-visible cancel (AbortSignal) arrives with streams and is
  bridged to `OP_CANCEL` there.

---

## 19. Invalid request handling, validation, and trust model

### 19.1 Principle

Every field the kernel reads from shared memory is validated before use. Invalid
input produces an explicit error completion for that request (async lane) or
`sm_status` (sync lane) — never UB, never a kernel trap, never silent
acceptance. The wasm sandbox additionally guarantees that even a hypothetical
validation miss is a catchable trap in the *kernel worker*, not memory
corruption (out-of-bounds wasm access traps — BROWSER_EXECUTION §4 Q7).

### 19.2 Validation checklist (kernel, per request; order matters)

1. `op` ∈ defined set for this ABI version → else `NODINE_ENOOP`.
2. `flags` unknown bits zero → else `NODINE_EINVAL_REQUEST`.
3. unused `a0..a4` zero where the op defines them unused → else
   `NODINE_EINVAL_REQUEST`.
4. `tx_off % 8 == 0` and `tx_off + tx_len ≤ TX_SIZE` → else
   `NODINE_EINVAL_REQUEST`; for path ops additionally `tx_len ≤ PATH_MAX`
   (`ENAMETOOLONG` if longer — a *domain* error, not a protocol error) and
   valid UTF-8 (kernel-side decode check; invalid → `EINVAL`).
5. Argument ranges per op (e.g. `READ.len_max ≤ area bound`, fd values u32) →
   `NODINE_EINVAL_REQUEST` / `EINVAL`.
6. fd resolution against the **calling process's** table → `EBADF`.
7. Path canonicalization + traversal protection (kernel VFS policy: project-root
   confinement, `..` normalization, no escape) → `EACCES`/`ENOENT` per VFS
   rules (Subagent D's `VFS_DESIGN.md` owns the policy; the ABI carries bytes).
8. Resource limits (fd table, kernel heap) → `EMFILE`/`ENOMEM`.

Completion-side (bridge) validation: `req_id` ∈ outstanding table; `op` echo
matches; `result_kind` legal for the request's mode (`RX_ARENA` illegal on the
sync mailbox; `SYNCRES` illegal on the completion ring) and
`result_off + result_len ≤ RX_SIZE` (kernel-generated refs are trusted-by-
construction, but the bridge checks bounds before reading — defense in depth).

### 19.3 Wrong-direction access

Ring roles are structural: the kernel only ever *reads* submit rings / *writes*
completion rings and mailboxes; the bridge only ever writes its submit ring /
reads its completion ring and mailbox. A direction violation is detectable by
the corrupted side through the validation above (e.g. a bridge-written
completion record fails the bridge's own outstanding-table check;
kernel-internal direction bugs fail kernel invariants). The spec-level
protection is the single-writer ownership table (§9.1) — any deviation is a bug
in one of the two trusted components, not an attack surface (§19.4).

### 19.4 Trust model (stated honestly for the adversarial reviewer)

The KSM is a single shared trust domain: **any agent holding the KSM reference
can physically write anywhere in it** — SABs provide no per-region protection.
The design's integrity therefore rests on:

1. **Trusted writers are our code only**: the kernel (Rust, audited) and the
   bridges (Rust, audited). User JavaScript **never receives a KSM reference**:
   the bridge keeps it module-private; the 39-binding seam exposes no raw
   buffers (Phase 1 surface, `poc/p1-poc/worker.mjs`); Node user code can create
   its *own* SABs (worker_threads) but cannot reach ours.
2. **The kernel validates all request content anyway** (§19.2): a buggy or
   hostile *bridge* cannot corrupt the kernel through malformed requests (all
   fields bounds- and type-checked); a hostile bridge writing kernel heap would
   require a kernel bug to matter, and the wasm sandbox turns OOB into traps.
3. **Untrusted-code containment**: user JS runs inside the runtime worker's JS
   realm; its only kernel reach is through Node API calls that the bridge
   validates as requests. Worker crashes cannot corrupt kernel state (kernel
   state lives in the KSM, which survives worker death — M13 VERIFIED).
4. Residual risk (documented, accepted): a fully compromised runtime worker
   (beyond a JS-level crash — e.g. an engine exploit) can scribble on the KSM.
   This is the same residual risk class as any single-address-space OS
   (unikernel). Browser-process-level site isolation is the outer boundary.
   Per-region hardware enforcement does not exist for SABs; a future
   hardening option (per-endpoint KSMs + kernel-side copy) is explicitly
   rejected for Phase 2 (it would double-copy every payload — §9.6 — for a
   threat model the TCB already covers). [DESIGN-PROPOSAL]

### 19.5 Stale / duplicate completion rule

The bridge accepts a completion iff `req_id` is in its outstanding table with
matching `op` echo; it then removes the id. Anything else is logged as
`ERR_NODINE_PROTOCOL` and dropped. Combined with the kernel's exactly-once
invariant (§18) and retire-before-reuse (§8.1), no stale completion can be
misattributed to a successor request.

---

## 20. ABI version negotiation and compatibility rules

### 20.1 Capability bitmap (`kernel_caps`, u32)

| Bit | Capability | v1.0 |
|---|---|---|
| 0 | `FS_V1` — the §7.3 filesystem op group | set |
| 1 | `WATCH` — §7.4 watcher ops | 0 (stub) |
| 2 | `PROC` — process lifecycle beyond READY/EXIT | 0 (stub) |
| 3 | `PERSIST` — P2-008 persistence ops | 0 |
| 4–7 | reserved (network groups, Phase 5) | 0 |
| 8–31 | reserved | 0 |

A bridge MUST check the group bit before mapping a Node API to ops in that
group; absence → the Node call fails with `ENOSYS`-shaped
`ERR_NODINE_NOT_IMPLEMENTED` (explicit, never silent).

### 20.2 Negotiation sequence (per endpoint, at bridge bring-up)

```
1. bridge reads KCTL: SeqCstLoad(kernel_state) first (must be RUNNING),
   then magic → mismatch: fatal ERR_NODINE_ABI_MAGIC (boot error, no traffic)
2. abi_version: bridge requires  kernel.major == bridge.major
                          and     kernel.minor >= bridge.minor
   else fatal ERR_NODINE_ABI_VERSION — explicit failure, never silent
3. bridge reads its endpoint-table slot (state RUNNING, generation match)
   else fatal ERR_NODINE_ENDPOINT_STATE
4. bridge submits OP_ABI_PROBE (sync) with a0 = bridge version
   → kernel replies 0 + {kernel version, caps} or NODINE_EABI_VERSION
   (double check: the KCTL could have been read during a kernel reboot window)
5. bridge submits OP_PROC_READY (async); kernel marks the endpoint RUNNING
6. any later suspicion (generation change detected at a wake point,
   §21.3) → re-run steps 1–4; on failure, all pending ops fail with
   ERR_NODINE_KERNEL_SHUTDOWN / _DEAD
```

Mismatch is always an explicit, named error — the "never silent" rule. There is
no compatibility shim, no auto-downgrade, no fallback encoding (directive §2.1).

### 20.3 Compatibility rules (what bumps what)

| Change | Version effect |
|---|---|
| Add ops (within allocated ranges §7.5) | **minor** +1 (old bridges unaffected: they never send new ops; new bridges probe caps first) |
| Add capability bits, new reserved-region use | **minor** +1 |
| Change any record layout, field width, offset, constant (sizes, strides, capacities, magic) | **major** +1 — hard incompatibility, explicit failure |
| Change op *semantics* of an existing op | **major** +1 |
| Change error-number values | **major** +1 (uv values are frozen by Node, §14) |
| Atomics-ordering relaxations | **major** +1 (proofs are ordering-sensitive) |

The ABI is **frozen at v1.0 for the duration of Phase 2 implementation** after
P2-002 review; the Rust artifact (§24) is the single source of truth — the
kernel and all bridges import it from `kernel/crates/kernel-abi` (P2-003) so
documentation and implementation cannot drift (directive §3 GATE C: "create an
ABI version definition in the source code").

---

## 21. Shutdown, kernel death, and worker death

### 21.1 Kernel shutdown (graceful drain vs immediate)

Initiated by the **host** over the control plane (postMessage — allowed class):

- **Graceful (`drain`):** kernel sets `kernel_state = DRAINING` (SeqCst), then:
  (a) rejects every *not-yet-dequeued* request with a `NODINE_ESHUTDOWN`
  completion (inline error completions are always writable — no payload);
  (b) completes all in-flight (dequeued) work normally; (c) flushes persistence
  (P2-008 hook); (d) sets `kernel_state = STOPPED`; (e) final
  `Atomics.notify` sweep over every endpoint's `cr_seq` and `sm_seq` (wakes all
  waiters — proof §16.5). Bridges observe DRAINING/STOPPED at their wake points
  and fail all pending + future calls with `ERR_NODINE_KERNEL_SHUTDOWN`. The
  host then terminates workers at its leisure.
- **Immediate:** host `terminate()`s the kernel worker (and/or runtime workers).
  Shared memory outlives the kernel worker (M13) but no agent services it;
  bridges detect via heartbeat stall (§21.3). Phase 2 treats immediate shutdown
  as session teardown (page close) — no in-session kernel restart (§21.3).

### 21.2 Process / endpoint shutdown (per-process drain)

`OP_PROC_EXIT` (async, bridge→kernel at Node process exit — `process.exit`,
natural exit, or fatal): kernel (a) stops dequeueing that endpoint, (b) fails
its not-yet-dequeued requests with `ECANCELED` completions (§18 — the
"no stale requests" rule: everything gets exactly one completion), (c) reclaims
**all** fds/OFDs/watchers of the pid, (d) frees the TX/RX in-flight payloads
(kernel-side accounting), (e) marks the endpoint `DEAD`, (f) notifies the host
(control plane) that the worker may be recycled (ADR-0003 full-teardown
recycling: a recycled endpoint gets a fresh `generation`; a stale bridge view
fails its next CAS/probe with `ERR_NODINE_ENDPOINT_STATE`).

### 21.3 Bridge-side detection of kernel death

The kernel heartbeat (`kernel_heartbeat_ms`, §5.3) is updated every pump pass
(≤ 250 ms, §10.5). The bridge checks it: (a) on every sync-wait rung (§11.3);
(b) on every async wake; (c) on a 250 ms bridge timer. Age > `KERNEL_DEATH_MS`
(1000 ms) ⇒ the bridge abandons (§11.5) / fails pending ops with
`ERR_NODINE_KERNEL_DEAD` and refuses new calls. A `generation` change observed
at any wake point ⇒ kernel rebooted ⇒ re-probe (§20.2 step 6) or fail. Phase 2
kernel death is a **session-fatal** event (no in-session kernel restart;
recovery-from-persistence is P2-008 scope) — stated honestly, not hidden.

### 21.4 Kernel-side detection of a dead Node worker (fd reclamation)

Three mechanisms, in order of authority:

1. **Clean exit:** `OP_PROC_EXIT` (§21.2) — the normal path.
2. **Host-authoritative notice (control plane):** the host owns worker
   lifecycle; it learns of worker death from `Worker.onerror` / completed
   `terminate()` and informs the kernel via postMessage; the kernel runs the
   §21.2 reclaim immediately. This is the *primary* detection for crashed or
   terminated workers.
3. **Heartbeat watchdog (backstop):** `ep_heartbeat_ms` (bridge-written, §5.5)
   stale > `WORKER_DEATH_MS` (default **30 000 ms**, generous by design) ⇒ the
   kernel flags the endpoint `SUSPECT` and asks the host (control plane) to
   probe (`postMessage` ping with 1 s deadline). A live-but-blocked worker
   (user `Atomics.wait`, long sync compute) cannot answer the ping — so the
   **host** (which owns the worker) makes the termination call, mirroring a
   parent's `SIGKILL` on a hung child; the kernel never reclaims on heartbeat
   staleness alone (a user process doing >30 s of blocking waits with zero
   event-loop turns is *alive* by Node semantics — this ambiguity is resolved
   by owner decision, documented divergence). On host confirmation, the kernel
   runs the §21.2 reclaim: fds, OFD refcounts, watchers, queued requests
   (failed with `ECANCELED` toward nobody — the consumer is gone; they are
   simply dropped from the outstanding table), arena accounting.

The reclaim is total: after it, no fd, byte, or request of the dead pid remains
in kernel state (acceptance-tested in P2-004: kill a worker with open fds and
pending ops; verify a fresh process sees a clean table).

---

## 22. Single-kernel-worker consequences (honest limitations)

Phase 2 has **one kernel worker, no kernel-side threads** (directive scope;
ADR-0002 notes cooperative scheduling as a known cost). Consequences the ABI
inherits — stated plainly:

1. **Serialized kernel dispatch.** All endpoints' ops execute one at a time on
   the kernel worker's thread. A CPU-heavy op (e.g. `OP_READDIR` batching
   thousands of entries) delays every other process by its duration. Bounds
   mitigate (caller-declared result caps §6.4 → every op is bounded work), and
   the round-robin pump (§10.5) interleaves endpoints per pass, but a single
   op's latency floor is its full execution time. Mitigation path (Phase 3+):
   kernel-side wasm threads over the same shared memory — the ABI needs **zero
   changes** (kernels that dequeue become N consumers of per-endpoint SPSC
   rings, which is safe: SPSC is per-ring; the kernel-internal state gets
   kernel-internal locks). Not a fallback — a planned scale-out.
2. **No preemption.** Long ops cannot be interrupted (§18); cancellation is
   cooperative.
3. **Kernel worker death is session-fatal in Phase 2** (§21.3). A wasm trap
   inside the kernel kills the kernel worker (BROWSER_EXECUTION §4 Q7);
   persistence replay recovery is P2-008 scope.
4. **Head-of-line deferral is per-endpoint FIFO** (§17.2): a payload op blocked
   on RX space also blocks later payload-free ops of the same endpoint
   (latency only, never deadlock — §16.3). A reordering dequeue (skip blocked
   ops) is a permissible future refinement (it only relaxes §12.4's ordering
   guarantee to libuv's own weaker "no cross-op ordering" — still
   Node-conformant); deliberately not in v1.0 to keep ordering deterministic
   and testable.
5. **One sync request in flight per endpoint** (§11.1): a Node process cannot
   parallelize `*Sync` calls — exactly like real Node, where sync calls occupy
   the caller's thread.
6. **Chunking granularity** (§11.6): huge single-buffer writes become ≤256 KiB
   chunks; concurrent writers to one file can interleave at chunk granularity.
   POSIX guarantees no atomicity for concurrent regular-file writes (only
   `O_APPEND` writes and `PIPE_BUF`-sized pipe writes are atomic), so this is a
   granularity divergence, not a semantic one — but it is observable to
   pathological concurrent-writer workloads. [INFERRED]
7. **EAGAIN on async overflow** (§14.5) — the deliberate bounded-queue
   divergence.

## 23. Performance budget

Anchors are the measured M1–M13 values (§4); targets are engineering envelopes
to be verified in P2-009 with the benchmark harness (directive §9). Assumptions
are labeled — no invented number is presented as measured.

| Quantity | Budget / target | Basis |
|---|---|---|
| Transport RTT, sync op (bridge submit → kernel completion → bridge wake) | **p50 ≤ 0.02 ms, p99 ≤ 0.1 ms, tail budget ≤ 1 ms** | M1 (p50 0.01, max 0.46 measured for the raw wait/notify round trip; +dispatch headroom). [TARGET anchored] |
| Kernel dispatch per simple op (validate + VFS lookup + reply) | ≤ 5 µs p50 | M7 (wasm-side atomic ping-pong p50 0.005 ms) bounds the wasm-side mechanics; VFS lookup cost is an [ASSUMPTION] — Rust in-wasm hash-trie lookup of µs class; must be measured in P2-009 |
| Sync `statSync` end-to-end (submit + dispatch + 144 B result + wake) | p50 ≤ 0.03 ms | M1 + dispatch target; 144 B copy negligible ([ASSUMPTION] wasm memcpy ≥ 1 GB/s → 144 B ≈ 0.14 µs; measure in P2-009) |
| Sync `readFileSync` small file (≤ 8 KiB) | p50 ≤ 0.05 ms | as above + payload copy [ASSUMPTION-labeled] |
| Sync `readFileSync` large (1 MiB) | ≤ 0.3 ms | 4 × 256 KiB chunks × (copy ≈ 0.25 ms @ 1 GB/s [ASSUMPTION] + transport 0.02) |
| Async submit (bridge-local until doorbell) | ≤ 2 µs | M1's submit half; stores + 2 atomics (measured class) |
| Async completion wake latency (`cr_seq` notify → bridge drain) | p50 ≤ 0.02 ms | M2 (waitAsync variant p50 0.015 ms measured) |
| Sync-op throughput, single process (sequential) | ≥ 50 000 ops/s | M1 total: 1000 calls / 10.5 ms = 95k/s including both waits — [VERIFIED class]; budget leaves dispatch headroom |
| Async-op sustained throughput per process | ≥ 100 000 ops/s (kernel-bound) | [TARGET; no kernel-side measurement exists yet — P2-009 must produce it; bridge-side submit is ~µs] |
| Arena copy budget per op | ≤ 2 copies × payload (read: VFS→area, area→Buffer; write: Buffer→TX, TX→VFS) | §9.6 design; copy bandwidth [ASSUMPTION] to measure |
| `OP_FSYNC` p95 (v1.0.1 scope note, G2b-C4) | ≤ 5 ms trivial batch; k-object batch ≈ k × (storage write+flush) serialized + 2 unmeasured hops (kernel→storage ring; notify→kernel wake) — re-derived in OPFS_PERSISTENCE §11 | F's anchors (0.405–1.5 ms small write+flush; documented 28 ms OPFS tails). The 0.785 ms row below measures the ring+VFS hop ONLY — the anchors measure different hops of the fsync chain and are not contradictory |
| OPFS-persistence-path ops (P2-008) | p50 ≈ 0.8 ms/cycle class | M8 (0.785 ms) — the async persistence layer's cost anchor; the in-memory VFS hot path must be ≥ 10× faster than this (verify P2-009) |
| Bootstrap: kernel init → KCTL RUNNING | ≤ 50 ms cold | [TARGET; anchored to worker spawn p50 7.9 ms (M13) + wasm instantiate (unmeasured — P2-003 will measure)] |
| Memory: transport pool reservation | 81 MiB address space, RSS ≈ touched pages only | §5.2 constants; M11 lazy-commit behavior |
| Heartbeat/timer overhead when idle | 1 futex wake / 250 ms | §10.5 (negligible; no busy spin — M3 rejected) |

Recording obligations carried to P2-009 (directive §9): browser version, OS,
hardware, build profile, wasm features, shared-memory size, workload, iterations,
p50/p95/p99 — the p1-c harness pattern (`poc/p1-c/run-all.cjs`) is the template.
Do not claim improvements until measured.

---

## 24. Rust ABI-definition artifact (normative)

This section is the **kernel-abi version definition** (directive §3 GATE C: "an
ABI version definition in the source code so the documentation and implementation
cannot silently drift apart"). It is destined verbatim for
`kernel/crates/kernel-abi/src/lib.rs` (P2-003) — this task creates no `kernel/`
tree. The kernel, the binding bridge, and any future endpoint implementation
import these constants; the tables in §5–§8 and §14 agree with this artifact
exactly. No compilation is performed in this task (no cargo/rustc in the
sandbox — the artifact is reviewed by inspection; P2-003 compiles it with
`static_assert_size!` failures as merge blockers).

```rust
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
pub const ABI_VERSION_MINOR: u16 = 1;
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
pub const KSM_DEFAULT_MAX_PAGES: u32 = 16384;

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
            | Op::Mkdir | Op::Rmdir | Op::Unlink | Op::Rename | Op::Readdir)
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
```

**Provenance of every value in the artifact:** magics and layout constants —
§5.2–§5.5; op ids — §7; flags/kinds/states — §6/§8.4/§11.5; NODINE codes —
§14.2; uv table — Appendix A (from `poc/p1-poc/uv-errno-linux.txt`, generated
from Node v26.13.0's vendored uv headers). The `assert!` blocks in §9 are the
drift guards the directive demands; P2-003 additionally adds a boot-time check
(kernel writes `ABI_MAGIC`/`ABI_VERSION` and reads them back before
`kernel_state = RUNNING`).

---

## 25. Directive §3 GATE C completeness checklist

Every item of the directive's "ABI specification requirements" list, with the
section that defines it:

| Directive item | Section |
|---|---|
| ABI magic | §5.3 (`ABI_MAGIC`), §24.1 |
| ABI version + version negotiation (mismatch = explicit failure, never silent) | §8.5, §20 (esp. §20.2 fatal errors) |
| Operation identifiers (stable numeric table: fs ops, process stubs, capability/init/shutdown ops) | §7 (full table), §7.5 (allocation plan) |
| Request identifiers (64-bit monotonic per producer; wraparound safety) | §8.1 |
| Process identifiers | §8.2 |
| File-descriptor identifiers (kernel namespace vs per-process table) | §8.3 |
| Request flags | §8.4 |
| Argument offsets + buffer lengths (arena, growth rules, ownership) | §6.1, §9 (esp. §9.2: no growth — fixed, versioned, lazily committed) |
| Result representation (status + inline vs area; max inline size; alignment) | §6.4 (inline ≤ 16 B; areas; 8-align) |
| Error codes (uv table + NODINE_* namespace; Node exception mapping ERR_FS_*/ERRNO shapes) | §14 (mapping table §14.4), Appendix A |
| Completion records (layout; request identification; ordering guarantees) | §6.2, §12.4 |
| Memory alignment rules (8-byte; why) | §5.1 |
| Ownership rules (single-writer per cell) | §9.1 |
| Buffer lifetime (producer-owned until completion; readable until retire) | §9.2–§9.4 |
| Cancellation (explicit op; in-flight behavior; no stale completions) | §18 |
| Backpressure (sync reserves vs async no-block + EAGAIN policy; Node justification) | §17 (sync: §17.1; async: §17.3; divergence: §14.5) |
| Invalid request handling (bad op/version/offset/alignment/direction → explicit error, never UB) | §19.1–§19.3 |
| Shutdown semantics (drain vs immediate; worker-death fd reclaim; kernel-death detection) | §21 |
| Formal memory layout (control block, per-process ring pairs, doorbells, arenas, kernel-reserved) with byte tables, widths, endianness | §5 (exact tables), little-endian §5.1 |
| Concurrency + memory-ordering guarantees for every shared field (Atomics ops, Rust orderings, spec citations) | §15.1, §15.4 |
| Happens-before: submission→observation; completion→retire | §15.3 (L1–L5) |
| Deadlock analysis (all wait edges, cycle proof, shutdown-wakes-waiters) | §16 |
| Ring mechanics (SPSC justification, index scheme, capacity, single kernel worker, wait disciplines, timeout error surface) | §10 (indices §10.1; capacities §10.6; kernel wait §10.5; sync wait + error §11.3) |
| ABI-version definition artifact (Rust, `kernel-abi`) | §24 |
| Performance budget (grounded in poc/p1-c, assumptions labeled) | §23 |
| Two endpoint classes through one ABI; SW path without a second ABI; SW never `Atomics.wait` | §3, §11–§13 |

## 26. Open questions for the main agent

1. **Endpoint count & KSM maximum sizing** (interlock with GATE B /
   `WORKER_MEMORY_MODEL.md`): `MAX_ENDPOINTS = 64`, `KSM_DEFAULT_MAX_PAGES =
   16384` (1 GiB) are v1.0 constants; confirm against the measured worker+memory
   budget before freezing (a constant change pre-freeze is free; post-freeze it
   is a minor bump only if the formula `ep_base = TRANSPORT_BASE + i×EP_STRIDE`
   and all sub-offsets are unchanged — capacity-only changes are minor).
2. **Watch semantics freeze timing**: `OP_WATCH_START/STOP/EVENT` record shapes
   are reserved (§7.4); freeze the event payload encoding when P2-005 decides
   inotify-vs-polling semantics (minor bump).
3. **worker_threads endpoints**: confirm Phase 3 target of one endpoint per
   thread (shared process fd table, §8.3) so SPSC-per-endpoint stands unchanged.
4. **Readdir granularity**: stateless cookie batching (§7.3) vs a `fs.Dir`
   handle-shaped `OP_OPENDIR/CLOSEDIR` pair (Subagent E's Dir semantics will
   tell; the cookie design supports Dir composition bridge-side — confirm with
   `NODE_FS_KERNEL_INTEGRATION.md` §7).
5. **One-shot read-file op**: `readFileUtf8`-style single-op fast path (Node's
   own utf8 fast route, `lib/fs.js:615-625`) would save 3 round trips per small
   file read; current design composes OPEN/FSTAT/READ/CLOSE (§11.6, §12).
   Decide after P2-009 measures the composed cost vs the 0.01 ms RTT anchor
   (adding `OP_READ_FILE_UTF8` later is a permitted minor bump).
6. **EAGAIN bounds**: `BRIDGE_QUEUE_MAX = 65536` (bridge-local) and ring 4096 —
   confirm against worst-case pnpm/Yarn workloads (P1-E data) before freeze.
7. **Kernel-worker restart** in-session (generation-based re-probe is specified,
   §21.3, but Phase 2 treats kernel death as session-fatal): decide whether
   P2-008's persistence layer must make kernel restart a Phase 2 requirement or
   a documented Phase 4 item.
8. **Path policy ownership**: path canonicalization/traversal rules are
   Subagent D's `VFS_DESIGN.md` domain; the ABI carries raw bytes and
   `ENAMETOOLONG`/UTF-8 validation only (§19.2 items 4/7) — confirm the split.
9. **JS-side `Atomics.wait` on a wasm-memory SAB**: spec-legal and both halves
   measured (E2 plain-SAB waits; E3 wasm-memory atomics) — P2-004's first test
   should close the loop with a literal JS `Atomics.wait` on the KSM view
   (5-minute check; flagged [DOCUMENTED]+[INFERRED] in §15.1).

## 27. Appendix A — libuv errno table

Verbatim from `poc/p1-poc/uv-errno-linux.txt` (generated from Node v26.13.0's
vendored `deps/uv` headers + system `errno.h` — the error model's frozen source,
§14.1; identical constants live in the Rust artifact §24.7):

```
E2BIG -7 argument list too long              ELOOP -40 too many symbolic links
EACCES -13 permission denied                 EMFILE -24 too many open files
EADDRINUSE -98 address already in use        EMSGSIZE -90 message too long
EADDRNOTAVAIL -99 address not available      ENAMETOOLONG -36 name too long
EAFNOSUPPORT -97 address family not supp.    ENETDOWN -100 network is down
EAGAIN -11 resource temporarily unavail.     ENETUNREACH -101 network unreachable
EAI_ADDRFAMILY -3000 af not supported        ENFILE -23 file table overflow
EAI_AGAIN -3001 temporary failure            ENOBUFS -105 no buffer space
EAI_BADFLAGS -3002 bad ai_flags value        ENODEV -19 no such device
EAI_BADHINTS -3013 invalid value for hints   ENOENT -2 no such file or directory
EAI_CANCELED -3003 request canceled          ENOMEM -12 not enough memory
EAI_FAIL -3004 permanent failure             ENONET -64 machine not on network
EAI_FAMILY -3005 ai_family not supported     ENOPROTOOPT -92 protocol unavailable
EAI_MEMORY -3006 out of memory               ENOSPC -28 no space on device
EAI_NODATA -3007 no address                  ENOSYS -38 function not implemented
EAI_NONAME -3008 unknown node or service     ENOTCONN -107 socket not connected
EAI_OVERFLOW -3009 argument buffer overflow  ENOTDIR -20 not a directory
EAI_PROTOCOL -3014 resolved protocol unknown ENOTEMPTY -39 directory not empty
EAI_SERVICE -3010 service not supported      ENOTSOCK -88 socket op on non-socket
EAI_SOCKTYPE -3011 socket type not supported ENOTSUP -95 operation not supported
EALREADY -114 connection already in progress EOVERFLOW -75 value too large
EBADF -9 bad file descriptor                 EPERM -1 operation not permitted
EBUSY -16 resource busy or locked            EPIPE -32 broken pipe
ECANCELED -125 operation canceled            EPROTO -71 protocol error
ECHARSET -4080 invalid Unicode character     EPROTONOSUPPORT -93 proto not supported
ECONNABORTED -103 software caused abort      EPROTOTYPE -91 wrong protocol type
ECONNREFUSED -111 connection refused         ERANGE -34 result too large
ECONNRESET -104 connection reset by peer     EROFS -30 read-only file system
EDESTADDRREQ -89 destination addr required   ESHUTDOWN -108 cannot send after shdn
EEXIST -17 file already exists               ESPIPE -29 illegal seek
EFAULT -14 bad address in syscall arg        ESRCH -3 no such process
EFBIG -27 file too large                     ETIMEDOUT -110 connection timed out
EHOSTUNREACH -113 host unreachable           ETXTBSY -26 text file busy
EINTR -4 interrupted system call             EXDEV -18 cross-device link not perm.
EINVAL -22 invalid argument                  UNKNOWN -4094 unknown error
EIO -5 i/o error
EISCONN -106 socket already connected
EISDIR -21 illegal operation on a directory
```

(The single-column source file is authoritative; this two-column rendering
preserves every name/value pair. 74 entries.)

## 28. Appendix B — worked byte-level examples

All values little-endian. Endpoint region base in these examples:
`EP = TRANSPORT_BASE + 0×EP_STRIDE = 0x0001_0000` (endpoint 0, pid 1). Hex
bytes were computed with a DataView in Node (exact; the generation snippet is
`new DataView(new ArrayBuffer(8)).setFloat64(0, v, true)`).

### B.1 `statSync("/project/data/test.txt")` — endpoint class 1

The file exists (written by a previous `writeFileSync`): size 8 (`"Nodine"`),
mode `0o100644` = 33188, ino 7, mtime 1780934400. The bridge (Rust, in the
process worker) performs:

**Step 1 — request record** into submit-ring slot 263 (`sr_tail = 0x0107`; slot
address `EP + 0xC_0100 + (0x107 & 0xFFF) × 64`):

```
EP+0xC_0100+0x107*64:
  +0x00  42 00 00 00 00 00 00 00   req_id   = 66 (0x42) — per-endpoint counter
  +0x08  04 01 00 00               op       = OP_STAT (0x0104)
  +0x0C  01 00 00 00               flags    = FLAG_MODE_SYNC
  +0x10  00 E0 03 00               tx_off   = 0x0003E000 (TX arena, sync reserve)
  +0x14  16 00 00 00               tx_len   = 22
  +0x18  00 00 00 00 00 00 00 00   a0..a4   = 0 (STAT takes no extra args)
  …     (a1–a4 all zero) …
  +0x38  00 00 00 00 00 00 00 00
```

**Step 2 — TX payload** at TX arena offset `0x3E000` (22 bytes, no NUL):

```
EP+0x0100+0x3E000:  2f 70 72 6f 6a 65 63 74 2f 64 61 74 61 2f 74 65 73 74 2e 74 78 74
                     /  p  r  o  j  e  c  t  /  d  a  t  a  /  t  e  s  t  .  t  x  t
```

**Step 3 — publish:** `c := load(sm_seq) (= 7)` · `store(sr_tail, 0x0108)` ·
`add(sr_sync_pending, +1)` · `add(kernel_doorbell, +1); notify(…, 1)` ·
`CAS(sm_state, IDLE→SUBMITTED)` · `wait(sm_seq, 7, rung=1ms)`.

**Step 4 — kernel** (priority scan sees `sr_sync_pending > 0`, finds the
MODE_SYNC entry at 0x107): validates (op known; flags known; `0x3E000 + 22 ≤
0x4_0000` ✓; path ≤ PATH_MAX ✓; UTF-8 ✓); stats the file; writes the 144-byte
result into the **sync-result area at offset 0** (single-occupancy — always
empty, §9.4):

```
EP+0x8_0100 + 0x00 (144 B = 18 × f64):
  [0] dev      00 00 00 00 00 00 f0 3f   (1.0)
  [1] mode     00 00 00 00 80 34 e0 40   (33188.0 = S_IFREG|0644)
  [2] nlink    00 00 00 00 00 00 f0 3f   (1.0)
  [3] uid      00 00 00 00 00 00 00 00   (0.0)
  [4] gid      00 00 00 00 00 00 00 00
  [5] rdev     00 00 00 00 00 00 00 00
  [6] blksize  00 00 00 00 00 00 b0 40   (4096.0)
  [7] ino      00 00 00 00 00 00 1c 40   (7.0)
  [8] size     00 00 00 00 00 00 20 40   (8.0)
  [9] blocks   00 00 00 00 00 00 f0 3f   (1.0)
  [10] atime_s  00 00 00 c0 b9 89 da 41  (1780934400.0)
  [11] atime_ns 00 00 00 00 00 00 00 00
  [12–17] mtime_s/nsec, ctime_s/nsec, birthtime_s/nsec — same pattern
```

**Step 5 — sync mailbox** (completion cell at `EP+0x0060`; the same completion
schema as §6.2):

```
  +0x00  08 00 00 00   sm_seq    = 8            (7 + 1; the doorbell the worker waits on)
  +0x04  02 00 00 00   sm_state  = COMPLETED
  +0x08  04 01 00 00   sm_op     = OP_STAT echo
  +0x10  42 00 00 00 00 00 00 00   sm_req_id = 66
  +0x18  00 00 00 00   sm_status = 0 (success)
  +0x1C  03 00 00 00   sm_kind   = RESULT_SYNCRES
  +0x20  00 00 00 00   sm_off    = 0
  +0x24  90 00 00 00   sm_len    = 144
  +0x28  (16 B inline — unused, zero)
```
then `notify(sm_seq, 1)`; `store(sr_head, 0x0108)`; `add(sr_sync_pending, −1)`.

**Step 6 — worker wakes** (`wait` returns `"ok"`), reads the mailbox, wraps the
result: `new Float64Array(ksm.buffer, EP + 0x8_0100 + 0, 18)` — the bytes *are*
Node's stat array (field order VERIFIED against
`vendor/node-v26.13.0/lib/internal/fs/utils.js:763-786`) →
`getStatsFromBinding` builds the `Stats` object → `CAS(sm_state,
COMPLETED→IDLE)` (retire) → `statSync` returns.

### B.2 `fs.promises.readFile("/project/data/async.txt", "utf8")` — endpoint class 2

The bridge composes `OP_OPEN` → `OP_FSTAT` → `OP_READ` → `OP_CLOSE` (§11.6); two
records shown. The file contains `"Async operation"` (15 bytes).

**B.2.1 OPEN (async):** request at submit slot `sr_tail = 0x0200`:

```
  +0x00  67 00 00 00 00 00 00 00   req_id = 0x67 (103)
  +0x08  00 01 00 00               op     = OP_OPEN (0x0100)
  +0x0C  00 00 00 00               flags  = 0 (MODE_ASYNC)
  +0x10  00 08 00 00               tx_off = 0x800   (async TX region)
  +0x14  17 00 00 00               tx_len = 23
  +0x18  00 00 00 00 00 00 00 00   a0 = O_RDONLY (0)
  +0x20  b6 01 00 00 00 00 00 00   a1 = mode 0o666 (438)
  +0x28.. +0x38  zeros             a2..a4 = 0
```
TX payload at `EP+0x0100+0x800`: `2f 70 72 6f 6a 65 63 74 2f 64 61 74 61 2f
61 73 79 6e 63 2e 74 78 74` (`/project/data/async.txt`). Publish:
`store(sr_tail, 0x0201)`; `add+notify(kernel_doorbell)`. The Node promise
(pending) is registered under `req_id 0x67`; the worker's event loop continues.

Kernel dequeues (FIFO, async lane), executes OPEN (allocates OFD kfd
`0x1234`, installs fd 3 in pid 1's table), writes the completion into
completion-ring slot `cr_tail = 0x0040`:

```
EP+0x10_0100 + (0x40 & 0xFFF)*64:
  +0x00  67 00 00 00 00 00 00 00   req_id      = 0x67
  +0x08  00 01 00 00               op echo     = OP_OPEN
  +0x0C  00 00 00 00               status      = 0
  +0x10  01 00 00 00               result_kind = RESULT_INLINE
  +0x14  04 00 00 00               result_len  = 4
  +0x18  00 00 00 00               result_off  = 0
  +0x1C  00 00 00 00               cflags      = 0
  +0x20  03 00 00 00 00 00 00 00 … inline      = fd 3 (u32 LE)
```
`store(cr_tail, 0x41)`; `store(cr_seq, +1)`; `notify(cr_seq, 1)`. The bridge's
persistent `waitAsync` fires (or returns not-equal), `drain()` retires the
record (`store(cr_head, 0x41)`), resolves the internal open-promise with fd 3.

**B.2.2 READ (async):** request `sr_tail = 0x0202`, `req_id = 0x69`:

```
  +0x00  69 00 00 00 00 00 00 00   req_id = 0x69
  +0x08  02 01 00 00               op     = OP_READ (0x0102)
  +0x0C  00 00 00 00               flags  = 0
  +0x10  00 00 00 00               tx_off = 0 (no payload)
  +0x14  00 00 00 00               tx_len = 0
  +0x18  03 00 00 00 00 00 00 00   a0 = fd 3
  +0x20  0f 00 00 00 00 00 00 00   a1 = len_max 15 (from the FSTAT size)
  +0x28  ff ff ff ff ff ff ff ff   a2 = pos −1 (use fd offset)
```

Kernel: reads 15 bytes from the file into the **RX arena** at bump offset 0
(8-aligned; free check `15 + 64 ≤ RX_SIZE` ✓), completes:

```
  completion record: req_id 0x69 · op OP_READ · status 0
                     kind = RESULT_RX_ARENA (2) · result_off = 0 · result_len = 15
  EP+0x4_0100 + 0:   41 73 79 6e 63 20 6f 70 65 72 61 74 69 6f 6e
                      A  s  y  n  c     o  p  e  r  a  t  i  o  n
```
The bridge's drain copies RX → a Node `Buffer` (§9.6 copy 2), joins the chunks,
decodes utf8, resolves the user promise; retires (`cr_head`); the RX bytes
become reusable by the kernel after the retire store (L3).

### B.3 SW ingress envelope (class 2 via MessageChannel, §13) — framing example

A (future, Phase 5) vTCP op submitted by the Service Worker travels as:

```
[ u32 total_len ][ RequestRecord — byte-identical to §6.1, FLAG_MODE_ASYNC ]
[ payload bytes (TX-equivalent) ]
```
and its completion returns as:

```
[ u32 total_len ][ CompletionRecord — byte-identical to §6.2 ][ payload bytes ]
```
written into the ingress endpoint's submit ring / read from its completion ring
by the kernel worker's driver — the same records, the same op table, the same
error codes. Only the transport differs (control-plane class, never hot path).

---

## 29. v1.0.2 delta (P2-004, ADR-0010) — additive only

The v1.0.1 text above is preserved verbatim (frozen history). This section is the
**only** v1.0.2 change set: a §20.3 row-1 additive minor bump (new op within the
allocation plan; no layout change; no semantic change to any existing op).
`abi_version` = `0x0001_0002`. v1.0.1-claiming bridges remain compatible
(`abi_compatible(0x0001_0002, 0x0001_0001)` = true).

**§7.5 (allocation plan, one-line extension):** `0x1000–0x10FF` — **diagnostic range**
(transport-validation ops; pure functions, zero kernel-state mutation, never mapped to
any Node syscall; see §7.7). `0x1100+` unallocated.

**§7.7 `OP_TRANSPORT_PROBE` (0x1000, both modes):** `a0` = result_len (0..16 →
`RESULT_INLINE` with result_len fill bytes; 17..`RX_SIZE−64` async /
17..`SYNCRES_SIZE−64` sync → `RESULT_RX_ARENA` / `RESULT_SYNCRES` payload of exactly
result_len bytes; out of range ⇒ `NODINE_EINVAL_REQUEST`); `a1` = fill byte (low 8
bits); `a2` = work_units (bounded in-kernel fill passes over kernel-heap scratch —
hard-capped inside the §11.3/F-B4 per-op budget — with a **cooperative cancel
re-scan** of the submit ring between units: SeqCst `sr_tail` load, then scan for a new
`OP_CANCEL` targeting this `req_id`; abort ⇒ `status = UV_ECANCELED`, kind NONE);
`a3`/`a4` = 0; TX optional, validated (§19.2 item 4 + §11.6 sync cap) and ignored.
Purpose: make §9.3's mandated ledger-wrap assertion and §17.2/§18 behaviors testable in
real Chromium before the VFS exists (ADR-0010 D1/D2).

**§17.3 (value fill):** `ASYNC_DEFER_LIMIT` = **40 pump passes** (40 × ≤ 250 ms §10.5
cadence = 10 s = `SYNC_TOTAL_TIMEOUT_MS`; per-endpoint deferral makes RX free space
monotonically grow, so the limit implies a genuinely wedged consumer).

**§18 (gap fills, adjudicated):** cancel-own completion status = **0** on both paths;
cooperative abort = `ECANCELED`, kind NONE, at the target's dequeue position; target
lookup scope = the submitting endpoint's unconsumed window; `OP_CANCEL` with
`FLAG_MODE_SYNC` ⇒ `NODINE_EINVAL_REQUEST`; the pre-pass cancel resolution (effects
at scan time, completions at each slot's FIFO position) is normative — it is the only
reading under which the "not yet dequeued" branch is reachable while §12.4.2 and
§12.4.5 both hold (P2_004_CANCELLATION.md reachability proof).

**§26.9 (resolved):** JS-side `Atomics.wait` on a wasm-memory SAB view — exercised and
verified by P2-003 (kernel driver doorbell waits over the KSM buffer, T0/T-bench, 9/9
PASS) and re-confirmed by the P2-004 suite.

**§B.1 (errata):** Appendix B.1 step 3 shows the pre-F-B2 publish order
(store→add→CAS); the normative §11.2 order is CAS(SUBMITTED) **before** ring publish.
The appendix example predates the v1.0.1 amendment and is historical.

---

*End of specification. Task P2-SA-B — deliverable for main-agent review and
P2-002 freeze. The author touched no files outside
`docs/phase2/KERNEL_ABI.md`. v1.0.2 delta appended by the main agent (P2-004).*

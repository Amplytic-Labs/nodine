# P2-006-007 — Browser Terminal + Process Supervisor (Agent C)

`poc/p2-007/` — the terminal page, the process supervisor, the supervisor-side
fs lane, the npm seeding pipeline and the host server. The runtime worker
(`public/runtime-worker.mjs`) is the **main agent's deliverable**; everything
here codes to the frozen contract below.

## Files

| file | role |
|---|---|
| `server.cjs` | COI static server + `/libsources.json` (verbatim Node v26.13.0 lib/) + one-time tarball unpack into `.cache/` with SHORT names (`npm/`, `pako/`, `noble/`, `shell/`; stamp-checked; gitignored via `poc/p2-007/.gitignore`) + `/npm-manifest.json` + `/npm-file?p=` + `/vendor/*` + `/unpkg/*`. Port 8897. |
| `public/terminal.html` | The page: 80×30 ANSI terminal, status bar (state · cwd · process count · `↳ N`), minimal line editor (NOT a shell), key routing, builtins, boot sequence, `window.__p2007`. |
| `public/supervisor.mjs` | `Term` (ANSI emulator), `parseCommandLine`, `Supervisor` (driver RPCs, kfs sync ops, seeding, exec resolution, process tree, relay, signals, shutdown). |
| `public/kernel-driver.js` | **Copy of p2-006's driver + two new RPCs** (`kfs-sync`, `stdin-feed`). Every p2-006 RPC is untouched. |
| `public/kernel_core.wasm` | Byte-identical copy of p2-006's released wasm (sha256 `2b514dd5dd92bbe9e737747e63719a874471eaff49044360a1c2aca63bd62ce3`). |
| `seed-bench.cjs` | Kernel-side seeding measurement (direct wasm, no RPC layer). |
| `supervisor-test.mjs` | 60-check integration test: REAL supervisor.mjs + REAL kernel-driver.js (vm-hosted) + scripted dummy runtime worker. |

Run: `node poc/p2-007/server.cjs` → `http://127.0.0.1:8897/`.
Test: `node poc/p2-007/supervisor-test.mjs` (needs the server running).

---

## 1. Message protocol (FROZEN — the runtime worker must match exactly)

### 1.1 Page → kernel-driver worker (postMessage, `tag`-matched RPC)

Every request carries `tag: number`; the reply echoes it. Unsolicited driver
events carry no tag.

| request | fields | reply |
|---|---|---|
| `boot` | `ksmMemory: WebAssembly.Memory(shared)`, `wasm: ArrayBuffer` (transferred copy) | `boot-ok {version, state, caps, maxEp, stride, stackPointer}` |
| `alloc-endpoint` | `pid: number`, `caps: 3` | `endpoint-allocated {index, generation, rc}` (negative index = failure) |
| `endpoint-exit` | `index` | `endpoint-exit-ack {rc}` |
| `endpoint-died` | `index, generation` | `endpoint-died-ack {rc}` (stale/DEAD → negative rc, harmless) |
| `shutdown` | — | `kernel-stopped {rc}` |
| `set-step-mode` / `step` / `set-watchdog` / `diag` | (p2-006 shapes, unchanged) | (unchanged) |
| **`kfs-sync`** (new) | `op: number`, `a0/a1/a2: BigInt` (`-1` allowed), `tx: Uint8Array\|null` (≤ 8128 B = ABI §11.6 sync payload cap) | `kfs-sync-result {status: number, inline: number, len: number, syncres: Uint8Array\|null}` |
| **`stdin-feed`** (new) | `bytes: Uint8Array` (capped to the 1 KiB scratch) | `stdin-fed {ok: number}` (accepted byte count) |

`kfs-sync` performs ONE §11.2 sync-mailbox op **on endpoint 0** (boot pid-1
context) inside the driver: SM_IDLE→SM_SUBMITTED CAS at `ep+0x60+4` →
submit-ring slot write at `ep+0xC0100+(tail&4095)*64` (`REQ_REQ_ID=0,
REQ_OP=8, REQ_FLAGS=0xc [FLAG_MODE_SYNC], REQ_TX_OFF=0x10, REQ_TX_LEN=0x14,
REQ_A0=0x18..`, TX at arena offset `0x3E000`, the 8 KiB guaranteed reserve) →
§10.4 doorbell bump (`Atomics.add+notify` on the u32 @ `0x2000`) →
`carapace_pump(now, 0)` → completion read from the sync mailbox
(`status@0x18, kind@0x1c, len@0x24, inline@0x28`) → SYNCRES payload
(`TRANSPORT_BASE + 0x80100`, `kind===3`) copied into a plain transferred
`Uint8Array` → SM_COMPLETED→SM_IDLE retire. `TRANSPORT_BASE = 0x10000`,
`EP_STRIDE = 0x141000`.

Ops used by the supervisor (a0/a1/a2 semantics per KERNEL_ABI):
`OPEN 0x0100 (a0=flags — create path uses 1|64|512, a1=mode, TX=path → inline=fd)`
· `CLOSE 0x0101 (a0=fd)` · `READ 0x0102 (a0=fd, a1=lenMax, a2=pos; −1 =
sequential → SYNCRES bytes)` · `WRITE 0x0103 (a0=fd, a1=pos(−1), TX=data →
inline=n)` · `STAT 0x0104` / `LSTAT 0x0105 (TX=path → SYNCRES 144 B, 18×f64:
[1]=mode [8]=size; ENOENT=−2)` · `MKDIR 0x0107 (a0=mode 0o777, TX=path)`
· `CHMOD 0x0122 (a0=mode, TX=path)` · `SYMLINK 0x0125 (TX=[target\0linkpath]
— TARGET FIRST)` · `READLINK 0x0126 (a0=cap, TX=path → SYNCRES target)`.

Unsolicited driver → page: `{type:'stdout'|'stderr', text}` (streaming-safe
UTF-8, per-stream decoders — the DRAIN PUMP; the terminal mixes both streams
like a real tty), `{type:'kernel-stopped'}`, `{type:'driver-error', error,
during}`, `{type:'driver-loop-died', error}`, `{type:'suspect', mask}`.

### 1.2 Page → runtime worker (main worker port) — `carapace-init`

```js
{
  type: 'carapace-init',
  ksmMemory: <WebAssembly.Memory>,   // the shared KSM (same object every process)
  epIndex: number, generation: number,
  pid: number,                       // EXTRA vs the minimal contract (diagnostics; additive)
  entry: string | null,              // absolute path of the main module (null = no module entry)
  argv: string[],                    // REST ARGS AFTER entry ([] when entry === null)
  argv0: string,                     // process.argv[0] ('node')
  env: object,                       // string → string
  cwd: string,                       // absolute
  stdioMode: 'inherit' | 'pipe' | 'ignore',
  channelPort: MessagePort,          // ALWAYS present (transferred)
  files: null,                       // the supervisor pre-seeds the VFS through endpoint 0
}
```

**The worker builds `process.argv = [argv0, ...(entry ? [entry] : []), ...argv]`.**
Examples (frozen): `npm install left-pad` → `entry=/usr/lib/node_modules/npm/bin/npm-cli.js`,
`argv=['install','left-pad']`; `sh -c 'echo hi'` → `entry=/bin/sh`,
`argv=['-c','echo hi']`; `node /project/main.js x` → `entry=/project/main.js`,
`argv=['x']`; bare `node` → `entry=null, argv=[]`.

### 1.3 Runtime worker → page (main worker port)

| message | fields | notes |
|---|---|---|
| `proc-ready` | — | after OP_PROC_READY lands |
| `proc-exit` | `code: number` | LAST message; the supervisor then reclaims the endpoint + terminates the worker. (p2-006's legacy `{type:'done', exitCode}` is also honored.) |
| `stdio-bytes` | `stream: 1\|2`, `bytes: ArrayBuffer` | pipe mode ONLY; relayed to the parent (below) |

Unknown/evidence messages are logged into `window.__p2007.logs`, never fatal.
Worker load failure (`onerror`) → terminal error line + the README hint + exit
1 + endpoint reclaim.

### 1.4 Channel port (supervisor ↔ worker MessageChannel)

Worker → supervisor (on its `channelPort`):
- `{type:'spawn', argv: string[], env: object|null, cwd: string|null, stdio: 'inherit'|'pipe'|'ignore'|array, shell: boolean}` — supervisor resolves + spawns (`shell:true` → `['/bin/sh','-c', argv.join(' ')]`), replies on the SAME port
- `{type:'child-stdin', pid, bytes: ArrayBuffer}` — data for a child's stdin (relayed as `stdin-bytes` on the child's channelPort)
- `{type:'kill', pid, sig}` — relayed as `signal` to that child

Supervisor → worker (on the worker's `channelPort`):
- `{type:'stdin-bytes', bytes: ArrayBuffer}` — pipe-mode stdin data
- `{type:'signal', sig: 'SIGINT'|'SIGTERM'}` — Ctrl+C goes to the foreground root AND every descendant
- `{type:'child-exit', pid, code}` — child exited
- `{type:'child-stdout', pid, bytes: ArrayBuffer}` / `{type:'child-stderr', pid, bytes}` — pipe-mode relay from the child's `stdio-bytes`
- `{type:'spawn-result', pid: number}` — success. **On failure:** `{type:'spawn-result', pid, error: string}` followed immediately by `{type:'child-exit', pid, code}` (127 not-found; 126 exec-format/unsupported interpreter; 71 no-free-endpoint). The pid is allocated but no process/endpoint is created.

Optional extensions the supervisor ALSO emits (a contract-exact worker may
ignore them): `{type:'stdin-eof'}` (parent closed the child's stdin — sent for
`{type:'child-stdin', pid, eof:true}`), `{type:'child-stdout'|'child-stderr',
pid, eof:true}` (stream EOF before `child-exit`).

stdio mapping (worker `spawn` request → process `stdioMode`): string passes
through; arrays are COARSE: any `pipe` at [1]/[2] → `'pipe'`, all-`ignore` →
`'ignore'`, else `'inherit'`. Per-fd granularity is the worker bridge's
concern (documented limitation).

### 1.5 Foreground model / stdin / signals

- One foreground ROOT process (terminal-spawned). Keystrokes while it runs:
  printable UTF-8, `Enter`→`\n`, `Backspace`→`0x7f`, `Tab`→`0x09`, arrows/
  Home/End/Delete as xterm ESC sequences, other Ctrl-letters as their byte.
  **Ctrl+C is SIGINT to the whole tree (byte NOT delivered)**; `^C` is echoed.
- inherit mode → bytes go through driver `stdin-feed` (kernel stdin FIFO at
  KSM scratch 0x2100, 1 KiB; `carapace_stdin_feed(0x2100, len)`; reads drain,
  EAGAIN when empty). pipe mode → `stdin-bytes` on the channelPort.
- The prompt returns when the foreground ROOT exits (status line `↳ N`);
  still-running descendants keep running (their inherit output keeps flowing).
- `proc-exit` → endpoint-died reclaim → parent notify (`child-exit`; the two
  EOF extensions first when applicable).

### 1.6 Terminal emulator (frozen minimal set)

80×30 viewport, ≥5000-line scrollback (cap 6000), streaming-safe UTF-8.
Handled: `\r` `\n` (ONLCR: row++, col=0) `\b`, `ESC[nA/B/C/D`, `ESC[K/1K/2K`,
`ESC[J/2J`, `ESC[?25l/?25h`, SGR `0 1 2 22 30-37 90-97 39` (16-color palette,
bold/dim); tabs expand to 8-col stops. Everything else (incl. `ESC[G`, `ESC[H`,
CUP, OSC, bg colors) is **stripped** — honest gap, see limitations.

Line editor: history ↑/↓ (200), ←/→ + Backspace + Delete, Home/End (also
Ctrl+A/E), Enter, Ctrl+C clears the line (prints `^C`), Ctrl+D on empty line
is a documented no-op, paste strips newlines. `cd` / `exit` / `export` /
`VAR=val` prefixes are builtins; a line containing `|` `<` `>` `&&` `;`
(outside quotes) prints `minimal terminal: pipes/redirects not supported yet`.
No var expansion, no globbing, no completion.

---

## 2. Exec rules (supervisor, in order)

1. `argv[0]` contains `/` → path (resolved against cwd); else PATH search
   (`/usr/bin:/bin`) via `STAT` (follows symlinks). No hit →
   `command not found` → 127.
2. Resolve the symlink chain (LSTAT + READLINK, ≤8 hops) — needed so node's
   module resolution sees the real file (`/usr/bin/npm` →
   `/usr/lib/node_modules/npm/bin/npm-cli.js`).
3. **Node self-stub**: resolved path ∈ {`/usr/bin/node`, `/bin/node`,
   `/usr/local/bin/node`} → node-worker with `entry = abs(argv[1])` or `null`
   (bare `node`), `argv0='node'`, `argv = argv.slice(2)`.
4. Read the first 128 B (OPEN O_RDONLY → READ a1=128 a2=−1 → CLOSE). `#!`
   interpreter parse (`env -S` aware): `node`, `carapace-node`,
   `/usr/bin/node`, `/usr/local/bin/node`, `/bin/node`, `*/node` → node-worker
   `entry = script path`, `argv0='node'`, `argv = argv.slice(1)`. Any other
   interpreter → `exec format not supported: <line>` → **126**.
5. No shebang: `.js`/`.mjs`/`.cjs` → node-worker (same shape as 4). Else
   `<argv0>: exec format error` → **126**.

`spawn('sh', ['-c', cmd])` works the same way: `/bin/sh` is a JS file with a
node shebang seeded in the VFS (its content is
`#!/usr/bin/env node\nrequire("/usr/lib/node_modules/shell/lib/index.js");\n`
— see `/npm-manifest.json` `entryStubs`), so exec turns it into a node worker
running `/bin/sh` with `argv = ['-c', cmd]`.

Deviation from real node (documented): `process.argv[1]` is the RESOLVED
symlink target, not the invoked path (real node keeps `/usr/bin/npm` in
argv[1] but loads the realpath; we only pass one path). Functionally required
for CJS relative-require resolution.

## 3. Seeding flow (first boot only)

1. `GET /npm-manifest.json` → `{files: [{path, size, mode, type:
   'file'|'symlink'|'dir', target?}], entryStubs: [{path, type, mode, size,
   content}]}` — the unpacked trees (`npm` 1943 files, `shell` 15, `pako` 33,
   `@noble/hashes` 247 under `/usr/lib/node_modules/**` — modes from disk
   `&0o777`, `0o755` forced for `bin`/`node-gyp-bin` paths), the
   `noble-hashes → @noble/hashes` alias symlink, `/usr/bin/npm` +
   `/usr/bin/npx` (symlinks → `../lib/node_modules/npm/bin/*-cli.js`),
   `/usr/bin/node` (stub file, `#!/usr/bin/env node\n`), `/bin/sh` (stub file
   requiring the vendored shell), `/home/user` (dir). Stub contents are
   inlined in `entryStubs` AND servable via `/npm-file?p=`.
2. Per entry (8-wide HTTP prefetch pipeline): `mkdir -p` parents (cached Set),
   symlinks via `OP_SYMLINK [target\0linkpath]`, files via
   `OP_OPEN(O_WRONLY|O_CREAT|O_TRUNC)` + chunked `OP_WRITE` + `OP_CLOSE`.
   Chunks are **≤ 8128 B per sync op** — the ABI §11.6 sync-payload cap
   (TX_SYNC_RESERVE − 64); the task brief's "≤60 KB" cannot cross the sync
   mailbox in one op (the kernel's TX arena reserve for MODE_SYNC is 8 KiB),
   so the supervisor bridge-chunks at 8128 B per the ABI.
   Create mode = `mode | 0o022` (pre-compensates the kernel's default umask
   0o022 so 644/755 land exactly); modes with group/other-write bits get an
   explicit `OP_CHMOD`. The real npm set is 644/755 only → zero CHMOD ops.
3. `/project/package.json` + `/project/main.js` demo workspace seeded after.
4. Progress in the status bar; `window.__p2007.seedMs`.

Base env for children: `HOME=/home/user`, `PATH=/usr/bin:/bin`,
`npm_config_registry=https://registry.npmjs.org/`, `npm_config_fetch_retries=2`,
`npm_config_loglevel=info`, `TERM=xterm-256color`, `LANG=en_US.UTF-8`,
`SHELL=/bin/sh`. **NO `CI` var** (deliberate — interactive terminal).

## 4. Measurements (this machine, real vendored tarballs)

- **manifest** (server log): 2244 entries — npm 1943, shell 15, pako 33,
  noble 247 files + 3 symlinks + 1 dir; 15,304,170 B (14.60 MB); manifest
  built in 19–28 ms; tarball unpack 110 ms cold / 0 ms stamped.
- **`seed-bench.cjs`** (direct wasm, exact supervisor op sequence):
  **2244 entries / 2240 files / 15.3 MB / 8441 sync ops → 51 ms** kernel-side;
  KSM 113.7 MiB after seeding; exec-resolution op sequence (PATH stat →
  lstat → readlink → stat resolved → open+read first 128 B) 0.24 ms;
  `npm-cli.js` mode `0o755` after umask compensation; first line
  `#!/usr/bin/env node`.
- **`supervisor-test.mjs`** (real supervisor.mjs + real kernel-driver.js code,
  same-thread RPC shim + real HTTP fetches): **boot 41–45 ms, seed 857–950 ms
  wall-clock** (fetch-bound: 2238 `/npm-file` requests ≈ 6.8–7.5 s of summed
  fetch time overlapped 8-wide; kernel-side 51 ms runs under the prefetch
  shadow). **60/60 checks pass** (tokenizer/metachars, boot, seeding, exec
  rules incl. 126/127, spawn-result/child-exit relay, pipe stdio relay,
  SIGINT tree, OFD-live back to 3 after reclaims, stdin-feed EAGAIN→feed→drain).
- **headless Chromium page smoke** (real page, COI on): `crossOriginIsolated:
  true`, boot 34 ms, **seed 2240 files (14.6 MB) in 6.3 s** (HTTP/1.1 — 6
  connections × 2238 small files dominates; see limitations), `cd`/assignment/
  metachar builtins verified, `node /project/main.js` → honest
  `[worker error] load failed` + README hint + `↳ 1` + prompt back, zero page
  errors.

## 5. Known limitations (honest)

- **No runtime worker yet** — `public/runtime-worker.mjs` is the main agent's
  deliverable; the page prints a clear error + contract pointer until it
  exists. Everything else is integration-tested with a scripted dummy worker.
- Minimal terminal, NOT a shell: no pipes/redirects/`&&`/`;`, no var
  expansion, no globbing, no completion, no job control beyond the single
  foreground tree.
- `Ctrl+D` cannot deliver real EOF: the kernel stdin FIFO has no EOF marker
  (bounded-drop device). It is a no-op on an empty line (logged) and
  delete-forward elsewhere. A real EOF needs a kernel-side flag (future work).
- ANSI: `ESC[G`/`ESC[H`/CUP and bg colors are stripped — progress bars that
  rely on absolute cursor addressing (rather than `\r`/`ESC[K`) show artifacts.
- Per-fd stdio granularity for worker-spawned children is coarse (see 1.4).
- `/bin/sh` requires the vendored `@yarnpkg/shell` — its tarball has UNBUNDLED
  deps (`tslib`, `@yarnpkg/fslib`, `@yarnpkg/parsers`, `chalk`, …), so a
  self-contained shell (bundling or shims) is the main agent's follow-up.
- `argv[1]` is the resolved symlink path (see §2 deviation).
- Keystrokes while scrolled-back: any key jumps to the bottom first.
- The line editor renders the prompt through the emulator grid; lines longer
  than 80 cols wrap (editor cursor math is column-based).
- Seeding fetches each file individually (`/npm-file?p=`); ~2240 requests on
  first boot → ~6.3 s in Chromium (HTTP/1.1 connection cap). Mitigations for
  later: HTTP/2 multiplexing, or one tarball + browser-side untar (the kernel
  side is only 51 ms — the wire is the bottleneck).
- Re-seeding happens on every page reload (no persistence channel yet —
  ADR-0009 future work).

## 6. `window.__p2007` (E2E hooks)

`{bootMs, seedMs, seedFetchMs, procs: Map, logs: [], pageErrors: [],
manifest: {files, bytes} | null, lastExit: number | null, stopped: boolean,
exitCode: number | null, bootError: string | null, term, supervisor}`
(superset of the required `{seedMs, bootMs, procs, pageErrors}`).

#!/usr/bin/env bash
# check-wasm-layout.sh — P2-004 build gate for the kernel wasm module footprint
# (hardened in P2-005-004b).
#
# Enforces the memory-layout invariants discovered in the P2-004 audit
# (WORKLOG P2-004-001; P2_004_CONCURRENCY.md §4; subagent B-1) plus the
# P2-005-004b module-footprint ceiling:
#   1. __stack_pointer is EXPORTED and initialized to KERNEL_STACK_TOP
#      (0x0508_0000 = KERNEL_HEAP_BASE + 128 KiB) — the wasm stack must live
#      in the kernel heap, never at linear offset 0 over the ABI map.
#   2. NO active data segments (an active segment would write the ABI map at
#      instantiation). Passive segments are allowed only if every memory.init
#      destination is >= KERNEL_HEAP_BASE (0x0506_0000).
#   3. No memory.init destination below KERNEL_HEAP_BASE.
#   4. __heap_base (the linker-computed END of the module [stack][data]
#      footprint) is EXPORTED and <= KH_BASE (0x0509_0040). P2-005-004b
#      found live that linking `alloc` + `kernel-vfs` grew .rodata to ~14 KB;
#      the P2-004 64 B reservation overflowed and the KH-header/epdir writes
#      at boot clobbered the module's own constants (carapace_init returned
#      ENOMEM with a half-booted VFS). The kernel-heap structures were moved
#      +64 KiB above the stack top; this check keeps every future build
#      inside that headroom.
#
# Usage: scripts/check-wasm-layout.sh [path-to-kernel_core.wasm]
# Exit 0 = layout OK; exit 1 = layout violation (build must fail).

set -euo pipefail

WASM="${1:-kernel/target/wasm32-unknown-unknown/release/kernel_core.wasm}"
KERNEL_HEAP_BASE=$((0x05060000))
KERNEL_STACK_TOP=$((0x05080000))
KH_BASE=$((0x05090040))

fail() { echo "LAYOUT-CHECK FAIL: $1" >&2; exit 1; }
command -v wasm-objdump >/dev/null 2>&1 || fail "wasm-objdump not found (npm i -g wabt)"

[ -f "$WASM" ] || fail "wasm not found: $WASM"

# --- 1. __stack_pointer exported + correct init value -----------------------
wasm-objdump -x "$WASM" > /tmp/wx.$$ || fail "wasm-objdump -x failed"
EXPS="$(cat /tmp/wx.$$)"
sp_line="$(echo "$EXPS" | grep 'stack_pointer.*init' || true)"
[ -n "$sp_line" ] || fail "no __stack_pointer global found"
sp_init="$(echo "$sp_line" | sed -n 's/.*init i32=\([0-9]*\).*/\1/p')"
[ "$sp_init" = "$KERNEL_STACK_TOP" ] \
  || fail "__stack_pointer init=$sp_init, expected $KERNEL_STACK_TOP (kernel heap)"
echo "$EXPS" | grep -q -- '-> "__stack_pointer"' \
  || fail "__stack_pointer is not exported"
rm -f /tmp/wx.$$

# --- 2. no active data segments ----------------------------------------------
# Active segments render as 'segment[N] <name> size=...' inside Data[] WITHOUT
# the word 'passive'; passive ones always carry 'passive'.
active="$(echo "$EXPS" | true; wasm-objdump -x "$WASM" | awk '/^Data\[/{ind=1;next} /^[A-Z]/{ind=0} ind && /segment\[/ && !/passive/' || true)"
[ -z "$active" ] || fail "active data segment present (would write linear memory at instantiation):
$active"

# --- 3. every memory.init destination >= KERNEL_HEAP_BASE -------------------
# The Start function's memory.init sequences as: i32.const DEST; i32.const SRC;
# i32.const LEN; memory.init. Take the FIRST i32.const of each group (DEST).
viol=0
wasm-objdump -d "$WASM" 2>/dev/null | awk '
  /i32\.const/ { match($0, /i32\.const [0-9]+/); c[++n] = substr($0, RSTART+10) }
  /memory\.init/ { if (n >= 3) { dest = c[n-2]; if (dest+0 < BASE) { print dest; bad=1 } } n=0 }
  END { exit bad ? 1 : 0 }' BASE="$KERNEL_HEAP_BASE" > /tmp/bad_dests.$$ || viol=1
if [ "$viol" -ne 0 ]; then
  fail "memory.init destination(s) below KERNEL_HEAP_BASE $(printf '0x%x' "$KERNEL_HEAP_BASE"): $(cat /tmp/bad_dests.$$)"
fi
rm -f /tmp/bad_dests.$$

# --- 4. module footprint ends below the KH header ---------------------------
# __heap_base = data_end as computed by wasm-ld (stack_size + data size).
hb_line="$(echo "$EXPS" | grep 'heap_base' || true)"
[ -n "$hb_line" ] || fail "__heap_base not found (not exported — build config)"
echo "$EXPS" | grep -q -- '-> "__heap_base"' \
  || fail "__heap_base is not exported"
hb_init="$(echo "$hb_line" | sed -n 's/.*init i32=\([0-9]*\).*/\1/p')"
[ -n "$hb_init" ] || fail "cannot parse __heap_base init value"
if [ "$hb_init" -lt "$KERNEL_STACK_TOP" ]; then
  fail "__heap_base=$hb_init < stack top $KERNEL_STACK_TOP (parser bug?)"
fi
if [ "$hb_init" -gt "$KH_BASE" ]; then
  fail "module footprint overflows the kernel heap: __heap_base=$(printf '0x%x' "$hb_init") > KH_BASE=$(printf '0x%x' "$KH_BASE") — the module [stack][data] block would overlap the KH header/epdir/blocks"
fi

echo "LAYOUT-CHECK OK: stack top=$(printf '0x%x' "$KERNEL_STACK_TOP") (kernel heap), no active data segments, all memory.init destinations >= $(printf '0x%x' "$KERNEL_HEAP_BASE"), module footprint (heap_base=$(printf '0x%x' "$hb_init")) < KH header $(printf '0x%x' "$KH_BASE")"

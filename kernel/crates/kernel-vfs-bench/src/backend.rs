//! Allocator backends + counters + clock (cfg-split per layer, B §10.1).
//!
//! - **Layer N (host)**: the counting shim wraps the system allocator
//!   (`std::alloc::System`) — structure-level native numbers. Block-level
//!   live/high-water are NOT available here (malloc internals); the
//!   requested-bytes counters stand in, clearly labeled.
//! - **Layer W (wasm32)**: the counting shim wraps the PRODUCTION `Kalloc`
//!   over the module's own linear memory `[__heap_base + RESULTS_RESERVE,
//!   mem_end)` with +16 MiB growth steps (same discipline as the kernel's
//!   `#[global_allocator]`) — block-exact `live`/`high_water`.
//!
//! Counters (both layers): `alloc_calls`, `free_calls`, `live_requested`,
//! `high_water_requested`; Layer W adds `block_live`/`block_high_water` from
//! the Kalloc state block.

#[cfg(target_arch = "wasm32")]
use core::sync::atomic::AtomicU32;
use core::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Clone, Copy, Default)]
pub struct AllocStats {
    pub alloc_calls: u64,
    pub free_calls: u64,
    pub live_requested: u64,
    pub high_water_requested: u64,
    /// Kalloc block-level live bytes (Layer W only; 0 on Layer N).
    pub block_live: u64,
    /// Kalloc block-level high-water (Layer W only; 0 on Layer N).
    pub block_high_water: u64,
}

pub static ALLOC_CALLS: AtomicU64 = AtomicU64::new(0);
pub static FREE_CALLS: AtomicU64 = AtomicU64::new(0);
pub static LIVE_REQ: AtomicU64 = AtomicU64::new(0);
pub static HIGHW_REQ: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// Layer N (host): system allocator behind the counting shim
// ---------------------------------------------------------------------------
#[cfg(not(target_arch = "wasm32"))]
pub mod backend {
    pub unsafe fn raw_alloc(size: usize, align: usize) -> *mut u8 {
        // `std::alloc::System` directly — `std::alloc::alloc` would dispatch
        // back through this very shim (infinite recursion).
        use std::alloc::{GlobalAlloc, Layout, System};
        match Layout::from_size_align(size, align.max(1)) {
            Ok(l) => unsafe { System.alloc(l) },
            Err(_) => core::ptr::null_mut(),
        }
    }
}

// ---------------------------------------------------------------------------
// The counting global allocator (both layers)
// ---------------------------------------------------------------------------
struct CountingShim;

#[cfg(not(target_arch = "wasm32"))]
impl CountingShim {
    unsafe fn do_alloc(&self, l: core::alloc::Layout) -> *mut u8 {
        let p = unsafe { backend::raw_alloc(l.size(), l.align()) };
        if !p.is_null() {
            count_alloc(l.size());
        }
        p
    }
}

#[cfg(not(target_arch = "wasm32"))]
unsafe impl core::alloc::GlobalAlloc for CountingShim {
    unsafe fn alloc(&self, l: core::alloc::Layout) -> *mut u8 {
        unsafe { self.do_alloc(l) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, l: core::alloc::Layout) {
        // System.dealloc DIRECTLY — std::alloc::dealloc would dispatch back
        // through this shim (the infinite-recursion bug this comment buried
        // once already: LLVM tail-looped it into a silent busy spin).
        use std::alloc::{GlobalAlloc, System};
        if !ptr.is_null() {
            count_free(l.size());
            unsafe { System.dealloc(ptr, l) }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[global_allocator]
static COUNTING: CountingShim = CountingShim;

#[inline]
fn count_alloc(size: usize) {
    ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
    let live = LIVE_REQ.fetch_add(size as u64, Ordering::Relaxed) + size as u64;
    HIGHW_REQ.fetch_max(live, Ordering::Relaxed);
}

#[inline]
fn count_free(size: usize) {
    FREE_CALLS.fetch_add(1, Ordering::Relaxed);
    LIVE_REQ.fetch_sub(size as u64, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// Layer W (wasm32): kalloc over the module heap + counting
// ---------------------------------------------------------------------------
#[cfg(target_arch = "wasm32")]
pub use backend::{KALLOC_BASE, RESULTS_RESERVE};

#[cfg(target_arch = "wasm32")]
pub mod backend {
    use super::*;
    use kernel_vfs::kalloc::Kalloc;

    /// Kalloc region base, set by `bench_init` (0 = not initialized).
    pub static KALLOC_BASE: AtomicU32 = AtomicU32::new(0);

    /// Bytes reserved at the start of the region for JS-side result buffers
    /// are accounted by the caller: the region starts at
    /// `__heap_base + RESULTS_RESERVE`.
    pub const RESULTS_RESERVE: u32 = 64 * 1024;

    /// Minimum growth step (eager reservation; untouched pages are free).
    const GROW_STEP: usize = 16 << 20;

    #[no_mangle]
    pub extern "C" fn memory_size_pages() -> u32 {
        core::arch::wasm32::memory_size(0) as u32
    }
    

    unsafe fn attach() -> Option<Kalloc> {
        let base = KALLOC_BASE.load(Ordering::Relaxed);
        if base == 0 {
            return None;
        }
        let sz = Kalloc::region_size(base as *mut u8)?;
        Kalloc::attach(base as *mut u8, sz).ok()
    }

    unsafe fn ensure_region(size: usize, align: usize) -> *mut u8 {
        let base = KALLOC_BASE.load(Ordering::Relaxed);
        if base == 0 {
            return core::ptr::null_mut();
        }
        let mut k = match unsafe { attach() } {
            Some(k) => k,
            None => return core::ptr::null_mut(),
        };
        if let Some(p) = unsafe { k.alloc_aligned(size, align) } {
            return p;
        }
        // Exhausted: grow the linear memory (+GROW_STEP or more) and retry.
        let cur_end =
            base as u64 + Kalloc::region_size(base as *mut u8).unwrap_or(0) as u64;
        let want = cur_end
            .saturating_add(size as u64 + align as u64 + 12)
            .max(cur_end + GROW_STEP as u64);
        if want > u32::MAX as u64 {
            return core::ptr::null_mut();
        }
        let need_pages = ((want + 65_535) / 65_536) as usize;
        let cur_pages = core::arch::wasm32::memory_size(0);
        if need_pages > cur_pages {
            if core::arch::wasm32::memory_grow(0, need_pages - cur_pages) == usize::MAX {
                return core::ptr::null_mut();
            }
        }
        let mem_end = (core::arch::wasm32::memory_size(0) as u32).saturating_mul(65_536);
        match unsafe { attach() } {
            Some(mut k2) => {
                if unsafe { k2.grow((mem_end - base) as usize) }.is_err() {
                    return core::ptr::null_mut();
                }
                match unsafe { attach() } {
                    Some(mut k3) => unsafe { k3.alloc_aligned(size, align) }
                        .unwrap_or(core::ptr::null_mut()),
                    None => core::ptr::null_mut(),
                }
            }
            None => core::ptr::null_mut(),
        }
    }

    pub unsafe fn raw_alloc(size: usize, align: usize) -> *mut u8 {
        unsafe { ensure_region(size, align) }
    }

    pub unsafe fn raw_dealloc(ptr: *mut u8, align: usize) {
        if ptr.is_null() {
            return;
        }
        if let Some(mut k) = unsafe { attach() } {
            if align > 8 {
                unsafe { k.dealloc_aligned(ptr) };
            } else {
                unsafe { k.dealloc(ptr) };
            }
        }
    }
}

#[cfg(target_arch = "wasm32")]
unsafe impl core::alloc::GlobalAlloc for CountingShim {
    unsafe fn alloc(&self, l: core::alloc::Layout) -> *mut u8 {
        let p = unsafe { backend::raw_alloc(l.size(), l.align()) };
        if !p.is_null() {
            count_alloc(l.size());
        }
        p
    }
    unsafe fn dealloc(&self, ptr: *mut u8, l: core::alloc::Layout) {
        if !ptr.is_null() {
            count_free(l.size());
            unsafe { backend::raw_dealloc(ptr, l.align()) };
        }
    }
}

#[cfg(target_arch = "wasm32")]
#[global_allocator]
static COUNTING_WASM: CountingShim = CountingShim;

/// Reset the per-bench counters (called by `run_bench` between benches so
/// one bench's peak cannot pollute the next bench's high-water — the B8
/// gate measures THE SOAK's high-water, not the process maximum). On wasm
/// the kalloc region's S_HIGHW is re-anchored to the current live (the
/// invariant `high_water >= live` is preserved).
pub fn reset_counters() {
    let live = LIVE_REQ.load(Ordering::Relaxed);
    ALLOC_CALLS.store(0, Ordering::Relaxed);
    FREE_CALLS.store(0, Ordering::Relaxed);
    LIVE_REQ.store(live, Ordering::Relaxed);
    HIGHW_REQ.store(live, Ordering::Relaxed);
    #[cfg(target_arch = "wasm32")]
    {
        let base = backend::KALLOC_BASE.load(Ordering::Relaxed);
        if base != 0 {
            if let Some(sz) = unsafe { kernel_vfs::kalloc::Kalloc::region_size(base as *mut u8) } {
                if let Ok(k) = unsafe { kernel_vfs::kalloc::Kalloc::attach(base as *mut u8, sz) } {
                    let used = k.live() as u32;
                    // S_HIGHW (state offset 32) := S_USED — re-anchor.
                    unsafe {
                        core::ptr::write_unaligned((base as *mut u8).add(32) as *mut u32, used);
                    }
                }
            }
        }
    }
}

/// Snapshot the counters.
pub fn alloc_stats() -> AllocStats {
    let mut s = AllocStats {
        alloc_calls: ALLOC_CALLS.load(Ordering::Relaxed),
        free_calls: FREE_CALLS.load(Ordering::Relaxed),
        live_requested: LIVE_REQ.load(Ordering::Relaxed),
        high_water_requested: HIGHW_REQ.load(Ordering::Relaxed),
        block_live: 0,
        block_high_water: 0,
    };
    #[cfg(target_arch = "wasm32")]
    {
        let base = backend::KALLOC_BASE.load(Ordering::Relaxed);
        if base != 0 {
            if let Some(sz) = unsafe { kernel_vfs::kalloc::Kalloc::region_size(base as *mut u8) } {
                if let Ok(k) = unsafe { kernel_vfs::kalloc::Kalloc::attach(base as *mut u8, sz) } {
                    s.block_live = k.live() as u64;
                    s.block_high_water = k.high_water() as u64;
                }
            }
        }
    }
    s
}

// ---------------------------------------------------------------------------
// Clock
// ---------------------------------------------------------------------------

/// Monotonic nanoseconds (f64). Host: `Instant`; wasm: the JS-imported
/// `performance.now()` (sub-µs precision in Node/Chromium). Per-op use of the
/// imported clock carries ~0.1–0.4 µs of call overhead per pair — benches at
/// that scale (B5) report it; batch gates (B1–B4, B10) amortize it to zero.
#[cfg(not(target_arch = "wasm32"))]
pub fn now_ns() -> f64 {
    use std::time::Instant;
    use std::sync::OnceLock;
    static T0: OnceLock<Instant> = OnceLock::new();
    let t0 = T0.get_or_init(Instant::now);
    t0.elapsed().as_secs_f64() * 1e9
}

#[cfg(target_arch = "wasm32")]
pub fn now_ns() -> f64 {
    #[link(wasm_import_module = "env")]
    extern "C" {
        fn now_ms() -> f64;
    }
    (unsafe { now_ms() }) * 1e6
}

//! Layer W wasm exports (only compiled for wasm32).
//!
//! Protocol (driven by poc/p2-005/run-bench.cjs):
//! 1. JS reads `__heap_base` (exported by the build config).
//! 2. `bench_init(results_off, results_cap)` — JS passes a results region
//!    [results_off, results_off+results_cap) INSIDE [__heap_base,
//!    __heap_base+RESULTS_RESERVE); the kalloc region is initialized at
//!    `__heap_base + RESULTS_RESERVE` (page-aligned upward) over the current
//!    memory; returns the kalloc base (0 = failure).
//! 3. `bench_run(id, out_off, out_cap) -> u32` — runs bench `id`, writes a
//!    NUL-terminated JSON line at `out_off`, returns its length (0 = no such
//!    bench; u32::MAX = internal panic marker — never expected).

#[cfg(target_arch = "wasm32")]
use core::sync::atomic::Ordering;

#[cfg(target_arch = "wasm32")]
#[no_mangle]
pub unsafe extern "C" fn bench_init(heap_base: u32, results_off: u32, results_cap: u32) -> u32 {
    use kernel_vfs::kalloc::Kalloc;
    // `heap_base` comes from the JS driver (it reads the exported
    // __heap_base global directly — reading it through an extern-static
    // inside wasm is unreliable: wasm globals are not linear memory).
    let base = (heap_base + crate::backend::RESULTS_RESERVE + 7) & !7;
    // results region must fit in the reserve below the kalloc base
    if results_off < heap_base
        || results_off + results_cap > base
        || results_cap < 16
    {
        return 0;
    }
    let mem_end = (core::arch::wasm32::memory_size(0) as u32).saturating_mul(65_536);
    if mem_end <= base {
        // grow one step so the region exists
        let need = ((base as usize) + (16 << 20) + 65_535) / 65_536;
        let cur = core::arch::wasm32::memory_size(0);
        if need > cur {
            if core::arch::wasm32::memory_grow(0, need - cur) == usize::MAX {
                return 0;
            }
        }
    }
    let mem_end = (core::arch::wasm32::memory_size(0) as u32).saturating_mul(65_536);
    match unsafe { Kalloc::init(base as *mut u8, (mem_end - base) as usize) } {
        Ok(_) => {
            crate::backend::KALLOC_BASE.store(base, Ordering::Relaxed);
            base
        }
        Err(_) => 0,
    }
}

#[cfg(target_arch = "wasm32")]
#[no_mangle]
pub unsafe extern "C" fn bench_run(id: u32, out_off: u32, out_cap: u32) -> u32 {
    let Some(line) = crate::run_bench(id) else {
        return 0;
    };
    let bytes = line.as_bytes();
    let n = bytes.len();
    if n + 1 > out_cap as usize {
        return u32::MAX; // caller must size the buffer
    }
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), out_off as *mut u8, n);
        (out_off as *mut u8).add(n).write(0);
    }
    n as u32
}



//! The global allocator the shipped binary installs: mimalloc, through `libmimalloc-sys`.
//!
//! Derived from mimalloc_rust (`src/lib.rs` of crates.io `mimalloc` 0.1.52, commit abcd2be6,
//! MIT, Copyright 2019 Octavian Oncescu); see ATTRIBUTIONS.md.
//!
//! The daemon's scans and cache warms allocate and free many short-lived multi-MB buffers across
//! dozens of threads; the macOS system allocator keeps that high-water mark resident (a 5 GB peak
//! settling at 1.6 GB of live data), while mimalloc returns freed pages to the OS.
//!
//! This is the `GlobalAlloc` shim of the `mimalloc` crate, written against `libmimalloc-sys`
//! directly, which [`crate::alloc_tag`] already needs. The library always compiles it, so the
//! dependency is used in every feature set; `main.rs` installs it unless the leak-check build
//! (`system-allocator`) asks for the system allocator.

use std::alloc::{GlobalAlloc, Layout};
use std::ffi::c_void;

/// mimalloc as a [`GlobalAlloc`].
pub struct MiMalloc;

// SAFETY: every method forwards to mimalloc's aligned entry points, which honour `layout.align()`
// and return null on failure as `GlobalAlloc` requires; a pointer handed to `dealloc` / `realloc`
// came from this allocator, so mimalloc owns it.
unsafe impl GlobalAlloc for MiMalloc {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: see the impl.
        unsafe { libmimalloc_sys::mi_malloc_aligned(layout.size(), layout.align()).cast() }
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: see the impl.
        unsafe { libmimalloc_sys::mi_zalloc_aligned(layout.size(), layout.align()).cast() }
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        // SAFETY: see the impl.
        unsafe { libmimalloc_sys::mi_free(ptr.cast::<c_void>()) }
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: see the impl.
        unsafe { libmimalloc_sys::mi_realloc_aligned(ptr.cast::<c_void>(), new_size, layout.align()).cast() }
    }
}

/// Return every free page mimalloc holds to the OS now, instead of after its purge delay.
///
/// A rescan, a read-stack build or a cache eviction frees GBs of transient buffers; mimalloc keeps
/// them in per-thread heaps and abandoned segments until a thread allocates again, so an idle daemon
/// stays at its high-water mark. Call it where a large transient ends. Cheap relative to the work
/// that precedes it (it walks page metadata, not the freed bytes); a no-op under the system
/// allocator, which `mi_collect` does not manage.
pub fn release_unused_memory() {
    // SAFETY: `mi_collect` only frees blocks mimalloc already considers free; it is safe from any thread.
    unsafe { libmimalloc_sys::mi_collect(true) }
}

/// Whether `ptr` points into mimalloc's heap: how a test proves which allocator a binary installed.
pub fn owned_by_mimalloc(ptr: *const u8) -> bool {
    // SAFETY: only compares the address against mimalloc's segment map; never dereferences it.
    unsafe { libmimalloc_sys::mi_is_in_heap_region(ptr.cast::<c_void>()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A zero-size layout still gets a distinct, aligned, freeable block (mimalloc's aligned entry
    /// points accept size 0), at the default and an over-aligned alignment.
    #[test]
    fn zero_size_layouts_get_aligned_freeable_blocks() {
        let a = MiMalloc;
        for align in [1usize, 16, 4096] {
            let layout = Layout::from_size_align(0, align).expect("layout");
            // SAFETY: each pointer is checked for null and alignment, never read, and freed once.
            unsafe {
                let p = a.alloc(layout);
                let z = a.alloc_zeroed(layout);
                assert!(!p.is_null() && !z.is_null());
                assert_ne!(p, z);
                assert_eq!(p as usize % align, 0);
                assert_eq!(z as usize % align, 0);
                a.dealloc(p, layout);
                a.dealloc(z, layout);
            }
        }
    }

    /// Committed bytes mimalloc holds, freed or not.
    fn committed_bytes() -> usize {
        let mut commit = 0usize;
        // SAFETY: every out-pointer is either null (ignored) or `&mut commit`, a live `usize`.
        unsafe {
            libmimalloc_sys::mi_process_info(
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &raw mut commit,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
        }
        commit
    }

    /// `release_unused_memory` is safe to call with freed blocks outstanding and never grows the
    /// committed set. How much of it the OS gets back depends on the allocator's purge policy, so
    /// the idle-memory band is asserted end to end by the hardening soak, not here.
    #[test]
    fn release_unused_memory_never_grows_the_committed_set() {
        const BLOCK: usize = 1 << 20;
        const BLOCKS: usize = 32;
        // SAFETY: each block is non-null (asserted), written within its length and freed once.
        let blocks: Vec<*mut u8> = (0..BLOCKS)
            .map(|_| unsafe {
                let p = libmimalloc_sys::mi_malloc(BLOCK).cast::<u8>();
                assert!(!p.is_null());
                std::ptr::write_bytes(p, 0xA5, BLOCK);
                p
            })
            .collect();
        for p in blocks {
            // SAFETY: allocated above by `mi_malloc`, freed exactly once.
            unsafe { libmimalloc_sys::mi_free(p.cast()) };
        }
        let before = committed_bytes();
        release_unused_memory();
        assert!(committed_bytes() <= before);
    }

    /// The shim round-trips every entry point, over-aligned layouts included.
    #[test]
    fn allocates_aligned_zeroed_and_reallocated_blocks() {
        let a = MiMalloc;
        for align in [8usize, 64, 4096] {
            let layout = Layout::from_size_align(100, align).expect("layout");
            // SAFETY: non-zero size; every pointer is checked, used within bounds and freed once.
            unsafe {
                let p = a.alloc_zeroed(layout);
                assert!(!p.is_null());
                assert_eq!(p as usize % align, 0);
                assert!(std::slice::from_raw_parts(p, 100).iter().all(|&b| b == 0));
                assert!(owned_by_mimalloc(p));
                std::ptr::write_bytes(p, 0xA5, 100);
                let q = a.realloc(p, layout, 10_000);
                assert!(!q.is_null());
                assert_eq!(q as usize % align, 0);
                assert!(std::slice::from_raw_parts(q, 100).iter().all(|&b| b == 0xA5));
                a.dealloc(q, Layout::from_size_align(10_000, align).expect("layout"));
            }
        }
    }
}

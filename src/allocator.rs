//! The global allocator the shipped binary installs: mimalloc, through `libmimalloc-sys`.
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

/// Whether `ptr` points into mimalloc's heap: how a test proves which allocator a binary installed.
pub fn owned_by_mimalloc(ptr: *const u8) -> bool {
    // SAFETY: only compares the address against mimalloc's segment map; never dereferences it.
    unsafe { libmimalloc_sys::mi_is_in_heap_region(ptr.cast::<c_void>()) }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                let q = a.realloc(p, layout, 10_000);
                assert!(!q.is_null());
                assert_eq!(q as usize % align, 0);
                a.dealloc(q, Layout::from_size_align(10_000, align).expect("layout"));
            }
        }
    }
}

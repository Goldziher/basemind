//! Keep the allocator's heap out of macOS's `IOAccelerator` accounting.
//!
//! mimalloc tags every anonymous mapping it makes on macOS with a Mach VM tag (`os_tag`), and its
//! default, 100, is `VM_MEMORY_IOACCELERATOR`. `footprint`, `vmmap` and Activity Monitor therefore
//! list the whole process heap under "IOAccelerator" -- a daemon that indexed a large repository
//! showed GBs of "GPU" memory while no Metal or CoreML device existed (the ONNX provider was
//! already pinned to CPU). Tag 254 is in the application-specific range (240-255), so the heap
//! reports as plain application memory.

#[cfg(target_os = "macos")]
const APPLICATION_SPECIFIC_VM_TAG: std::ffi::c_long = 254;

/// Retag mimalloc's mappings. Idempotent and cheap; call it before the first sizeable allocation.
/// mimalloc reads the option on every `mmap`, so mappings created earlier keep tag 100 (a few
/// hundred KB at most). A no-op off macOS, where the tag does not exist.
pub fn retag_heap_pages() {
    #[cfg(target_os = "macos")]
    // SAFETY: `mi_option_set` only stores an integer in mimalloc's option table.
    unsafe {
        libmimalloc_sys::mi_option_set(libmimalloc_sys::mi_option_os_tag, APPLICATION_SPECIFIC_VM_TAG);
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    /// Virtual bytes `vmmap -summary` attributes to `IOAccelerator` for this process (reserved
    /// address space excluded). A few KB survive from mimalloc's bootstrap, which runs before
    /// anything can retag it, so callers compare against a threshold, not zero.
    fn io_accelerator_virtual_bytes() -> Option<u64> {
        let out = std::process::Command::new("vmmap")
            .args(["-summary", &std::process::id().to_string()])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let size = text
            .lines()
            .find(|l| l.starts_with("IOAccelerator") && !l.contains("(reserved)"))
            .and_then(|l| l.split_whitespace().nth(1))
            .map(|tok| {
                let (num, unit) = tok.split_at(tok.len() - 1);
                let scale = match unit {
                    "K" => 1u64 << 10,
                    "M" => 1 << 20,
                    "G" => 1 << 30,
                    _ => 1,
                };
                (num.parse::<f64>().unwrap_or(0.0) * scale as f64) as u64
            });
        Some(size.unwrap_or(0))
    }

    #[test]
    fn retagged_heap_pages_do_not_show_up_as_ioaccelerator() {
        retag_heap_pages();
        assert_eq!(
            // SAFETY: reads an integer from mimalloc's option table.
            unsafe { libmimalloc_sys::mi_option_get(libmimalloc_sys::mi_option_os_tag) },
            APPLICATION_SPECIFIC_VM_TAG
        );
        // Straight to mimalloc: a fresh arena mapping, tagged by the option just set. Touch every
        // page so the region is resident and unmistakable in the summary.
        // SAFETY: allocate, write within bounds, free; the pointer is checked for null.
        unsafe {
            let len = 64 << 20;
            let p = libmimalloc_sys::mi_malloc(len).cast::<u8>();
            assert!(!p.is_null());
            std::ptr::write_bytes(p, 1, len);
            let tagged = io_accelerator_virtual_bytes();
            libmimalloc_sys::mi_free(p.cast());
            if let Some(bytes) = tagged {
                assert!(bytes < 32 << 20, "heap still tagged IOAccelerator: {bytes} bytes");
            }
        }
    }
}

//! Step 8：Rust 側零 allocation ≠ 零延遲尖峰。
//!
//! `Box<[OrderSlot]>` 分配咗 64 MB，但 kernel 係 lazy map 嘅 —— 第一次 touch
//! 每一頁都係一次 minor page fault（~1 μs）。所以 init 之後要：
//!
//! 1. **Pre-fault**：逐頁寫一次，強制頁面進駐
//! 2. **`mlock`**：釘住，唔畀 swap 出去
//! 3. **`MADV_HUGEPAGE`**：64 MB 用 4 KB page 要 16384 個 TLB entry（穩爆 dTLB），
//!    用 2 MB huge page 只要 32 個
//!
//! 呢個 module 係整個 engine 唯一嘅 `unsafe` —— 而且係同 OS 打交道，
//! 唔係資料結構 invariant。

// macOS 同 Linux 都有 mlock；MADV_HUGEPAGE 就只有 Linux 先有
// （macOS 嘅 superpage 要喺 mmap 嗰陣用 VM_FLAGS_SUPERPAGE_SIZE_ANY 開，
//  唔可以事後 madvise）。
#[cfg(any(target_os = "linux", target_os = "macos"))]
use core::ffi::c_void;

#[cfg(any(target_os = "linux", target_os = "macos"))]
unsafe extern "C" {
    fn mlock(addr: *const c_void, len: usize) -> i32;
}

/// Linux `MADV_HUGEPAGE`
#[cfg(target_os = "linux")]
const MADV_HUGEPAGE: i32 = 14;

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn madvise(addr: *mut c_void, len: usize, advice: i32) -> i32;
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemReport {
    pub bytes_prefaulted: usize,
    pub locked: bool,
    pub hugepage_advised: bool,
}

/// Touch initialized values at roughly 4 KiB intervals before serving traffic.
/// Typed copies avoid interpreting struct padding as initialized bytes.
/// See https://doc.rust-lang.org/std/ptr/fn.read_volatile.html#safety
pub fn prefault<T: Copy>(buf: &mut [T]) -> usize {
    let size = core::mem::size_of::<T>();
    if size == 0 {
        return 0;
    }
    let stride = (4096 / size).max(1);
    // The last element also touches a trailing page when the allocation is unaligned.
    let indices = (0..buf.len())
        .step_by(stride)
        .chain(buf.len().checked_sub(1));
    for i in indices {
        let value = &mut buf[i];
        // SAFETY: value is aligned, initialized and exclusively borrowed.
        // T: Copy prevents duplicate ownership. Volatile keeps the OS page touch
        // from being optimized away; this is startup work, not a hot-path shortcut.
        unsafe {
            core::ptr::write_volatile(value, core::ptr::read_volatile(value));
        }
    }
    core::mem::size_of_val(buf)
}

/// 釘住頁面（需要 `RLIMIT_MEMLOCK`；唔夠權限會失敗，返 false）。
pub fn lock<T>(buf: &mut [T]) -> bool {
    let len = core::mem::size_of_val(buf);
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        // SAFETY: 指標同長度嚟自一個有效 slice。
        unsafe { mlock(buf.as_ptr() as *const c_void, len) == 0 }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = len;
        false
    }
}

/// 建議 kernel 用 transparent huge page。
pub fn advise_hugepage<T>(buf: &mut [T]) -> bool {
    let len = core::mem::size_of_val(buf);
    #[cfg(target_os = "linux")]
    {
        // SAFETY: 同上。madvise 失敗只係冇 hugepage，唔影響正確性。
        unsafe { madvise(buf.as_mut_ptr() as *mut c_void, len, MADV_HUGEPAGE) == 0 }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = len;
        false
    }
}

/// 一次過做齊三樣。
pub fn warm<T: Copy>(buf: &mut [T]) -> MemReport {
    let hugepage_advised = advise_hugepage(buf);
    let bytes_prefaulted = prefault(buf);
    let locked = lock(buf);
    MemReport {
        bytes_prefaulted,
        locked,
        hugepage_advised,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefault_reports_byte_count_and_preserves_content() {
        let mut v: Vec<u64> = (0..10_000).collect();
        let n = prefault(&mut v);
        assert_eq!(n, 10_000 * 8);
        assert_eq!(v[0], 0);
        assert_eq!(v[9_999], 9_999);
    }

    #[test]
    fn prefault_preserves_padded_values() {
        #[derive(Clone, Copy, Debug, PartialEq)]
        #[repr(C)]
        struct Padded {
            tag: u8,
            value: u64,
        }
        let mut values = [Padded { tag: 7, value: 42 }; 300];
        let bytes = prefault(&mut values);
        assert_eq!(bytes, core::mem::size_of_val(&values));
        assert!(values.iter().all(|v| *v == Padded { tag: 7, value: 42 }));
        assert_eq!(prefault(&mut [(); 2]), 0);
        assert_eq!(prefault::<u64>(&mut []), 0);
    }

    #[test]
    fn warm_does_not_corrupt_data() {
        let mut v: Vec<u32> = (0..5_000).collect();
        let r = warm(&mut v);
        assert_eq!(r.bytes_prefaulted, 20_000);
        for (i, x) in v.iter().enumerate() {
            assert_eq!(*x, i as u32);
        }
    }
}

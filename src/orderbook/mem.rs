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

#[inline]
fn as_bytes_mut<T>(buf: &mut [T]) -> &mut [u8] {
    let len = core::mem::size_of_val(buf);
    // SAFETY: 只係當成 byte 睇同一段記憶體，長度用 size_of_val 算，冇越界。
    unsafe { core::slice::from_raw_parts_mut(buf.as_mut_ptr() as *mut u8, len) }
}

/// 逐頁做一次 read-modify-write（寫返原值），強制 page fault in。
/// 用 volatile 防止 optimizer 當成 no-op 消走。
///
/// Stride 用 4 KiB：喺 16 KiB page 嘅平台（Apple Silicon）會多寫幾次，
/// 但一定唔會漏頁。行一次 init 而已，唔值得為咗慳幾條 store 而去
/// `sysconf(_SC_PAGESIZE)`。
pub fn prefault<T>(buf: &mut [T]) -> usize {
    let bytes = as_bytes_mut(buf);
    let len = bytes.len();
    let ptr = bytes.as_mut_ptr();
    let mut off = 0usize;
    while off < len {
        // SAFETY: off < len，指標喺 buffer 內。
        unsafe {
            let p = ptr.add(off);
            let v = core::ptr::read_volatile(p);
            core::ptr::write_volatile(p, v);
        }
        off += 4096;
    }
    len
}

/// 釘住頁面（需要 `RLIMIT_MEMLOCK`；唔夠權限會失敗，返 false）。
pub fn lock<T>(buf: &mut [T]) -> bool {
    let bytes = as_bytes_mut(buf);
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        // SAFETY: 指標同長度嚟自一個有效 slice。
        unsafe { mlock(bytes.as_ptr() as *const c_void, bytes.len()) == 0 }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = bytes;
        false
    }
}

/// 建議 kernel 用 transparent huge page。
pub fn advise_hugepage<T>(buf: &mut [T]) -> bool {
    let bytes = as_bytes_mut(buf);
    #[cfg(target_os = "linux")]
    {
        // SAFETY: 同上。madvise 失敗只係冇 hugepage，唔影響正確性。
        unsafe { madvise(bytes.as_mut_ptr() as *mut c_void, bytes.len(), MADV_HUGEPAGE) == 0 }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = bytes;
        false
    }
}

/// 一次過做齊三樣。
pub fn warm<T>(buf: &mut [T]) -> MemReport {
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
    fn warm_does_not_corrupt_data() {
        let mut v: Vec<u32> = (0..5_000).collect();
        let r = warm(&mut v);
        assert_eq!(r.bytes_prefaulted, 20_000);
        for (i, x) in v.iter().enumerate() {
            assert_eq!(*x, i as u32);
        }
    }
}

//! 一個會數 allocation 嘅 global allocator。
//!
//! 「零 allocation」係一個會慢慢腐蝕嘅性質 —— 今日做到，聽日有人加一句
//! `format!` 或者換咗個 crate 就冇咗。所以要有機器幫你守住。
//!
//! 用法：
//! ```ignore
//! #[global_allocator]
//! static A: alloc_guard::Counting = alloc_guard::Counting;
//!
//! alloc_guard::arm();
//! //  ... hot path ...
//! let (allocs, bytes) = alloc_guard::disarm();
//! assert_eq!(allocs, 0);
//! ```
//!
//! 注意：`arm()` 之後**唔可以** panic —— panic 自己會 allocate。

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub static ARMED: AtomicBool = AtomicBool::new(false);
pub static ALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
pub static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);

pub struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ARMED.load(Ordering::Relaxed) {
            ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
            ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if ARMED.load(Ordering::Relaxed) {
            ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
            ALLOC_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

pub fn arm() {
    ALLOC_COUNT.store(0, Ordering::SeqCst);
    ALLOC_BYTES.store(0, Ordering::SeqCst);
    ARMED.store(true, Ordering::SeqCst);
}

/// 返回 (allocation 次數, 總 bytes)
pub fn disarm() -> (u64, u64) {
    ARMED.store(false, Ordering::SeqCst);
    (
        ALLOC_COUNT.load(Ordering::SeqCst),
        ALLOC_BYTES.load(Ordering::SeqCst),
    )
}

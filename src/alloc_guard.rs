//! 一個會數 allocation 嘅 global allocator。
//!
//! 「零 allocation」係一個會慢慢腐蝕嘅性質 —— 今日做到，聽日有人加一句
//! `format!` 或者換咗個 crate 就冇咗。所以要有機器幫你守住。
//!
//! # 點解 counter 係 thread-local
//!
//! `cargo test` 預設並行跑 test，每個 test 一條 thread。如果 counter 係全域，
//! thread A `arm()` 咗之後，thread B 起自己個 fixture 嘅 allocation 就會記落
//! A 個數 —— 結果係「有時綠有時紅」嘅 flaky test。
//!
//! 用 thread-local 之後，每條 thread 只數自己嘅 allocation，
//! 唔使逼人 `--test-threads=1`。
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
use std::cell::Cell;

thread_local! {
    // const-init + 冇 Drop 嘅型別 -> 唔會 lazy 註冊、唔會 allocate，
    // 所以喺 allocator 入面存取佢唔會遞迴。
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
    static BYTES: Cell<u64> = const { Cell::new(0) };
}

pub struct Counting;

#[inline(always)]
fn record(size: usize) {
    // try_with：thread 拆卸期間 TLS 可能已經銷毀，呢度絕對唔可以 panic
    let _ = ARMED.try_with(|armed| {
        if armed.get() {
            let _ = ALLOCS.try_with(|c| c.set(c.get() + 1));
            let _ = BYTES.try_with(|c| c.set(c.get() + size as u64));
        }
    });
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc(layout) }
    }

    /// `vec![0u8; n]` 行呢條路，唔行 `alloc`。
    /// 之前個版本冇覆蓋佢 —— 即係話 zeroed allocation 會被漏數。
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record(new_size);
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

/// 開始計數（只影響當前 thread）。
pub fn arm() {
    ALLOCS.with(|c| c.set(0));
    BYTES.with(|c| c.set(0));
    ARMED.with(|a| a.set(true));
}

/// 停止計數，返回 (allocation 次數, 總 bytes)。
pub fn disarm() -> (u64, u64) {
    ARMED.with(|a| a.set(false));
    (ALLOCS.with(|c| c.get()), BYTES.with(|c| c.get()))
}

/// 唔停低計數睇一眼當前數字。
pub fn peek() -> (u64, u64) {
    (ALLOCS.with(|c| c.get()), BYTES.with(|c| c.get()))
}

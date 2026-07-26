//! Step 8 嘅守門員：斷言 hot path 一個 malloc 都冇。
//!
//! 呢個 test 用自己嘅 global allocator，所以唔會影響 library 嘅 consumer。
//! 一旦有人喺 hot path 加返 `format!`、`Vec`、或者換咗個會 allocate 嘅
//! dependency，CI 即刻紅。
//!
//! Counter 係 thread-local，所以 test 可以照並行跑 —— 唔使
//! `--test-threads=1`。呢個 file 入面有個 test 專門守住呢件事。

use orderbook::alloc_guard;
use orderbook::{BookConfig, ClientOrderId, LevelInfo, NewOrder, OrderBook, OrderId, Side, TradeBuf};

#[global_allocator]
static ALLOC: alloc_guard::Counting = alloc_guard::Counting;

const TS: i64 = 1_700_000_000_000_000_000;

fn book(max_orders: usize) -> OrderBook {
    let mut b = OrderBook::with_config(BookConfig {
        max_orders,
        base_price: 0,
        tick_size: 1,
        num_ticks: 1024,
    });
    b.warm_up();
    b
}

#[test]
fn submitting_resting_orders_allocates_nothing() {
    let mut ob = book(20_000);
    let mut trades = TradeBuf::with_capacity(64);

    alloc_guard::arm();
    for i in 0..20_000u64 {
        trades.clear();
        let req = NewOrder::limit(ClientOrderId(i), Side::Buy, (i % 900) as i64, 10);
        let _ = ob.submit(&req, &mut trades, TS);
    }
    let (allocs, bytes) = alloc_guard::disarm();

    assert_eq!(allocs, 0, "submit allocated {allocs} times ({bytes} bytes)");
}

#[test]
fn cancelling_allocates_nothing() {
    let mut ob = book(20_000);
    let mut trades = TradeBuf::with_capacity(64);
    let mut ids: Vec<OrderId> = Vec::with_capacity(20_000);
    for i in 0..20_000u64 {
        trades.clear();
        ids.push(
            ob.submit(
                &NewOrder::limit(ClientOrderId(i), Side::Buy, (i % 900) as i64, 10),
                &mut trades,
                TS,
            )
            .unwrap(),
        );
    }

    alloc_guard::arm();
    for id in &ids {
        ob.cancel(*id).unwrap();
    }
    let (allocs, bytes) = alloc_guard::disarm();

    assert_eq!(allocs, 0, "cancel allocated {allocs} times ({bytes} bytes)");
}

#[test]
fn matching_allocates_nothing() {
    let mut ob = book(40_000);
    let mut trades = TradeBuf::with_capacity(256);
    for i in 0..20_000u64 {
        trades.clear();
        ob.submit(
            &NewOrder::limit(ClientOrderId(i), Side::Buy, 100, 5),
            &mut trades,
            TS,
        )
        .unwrap();
    }

    alloc_guard::arm();
    for i in 0..10_000u64 {
        trades.clear();
        let _ = ob.submit(
            &NewOrder::market(ClientOrderId(1_000_000 + i), Side::Sell, 7),
            &mut trades,
            TS,
        );
    }
    let (allocs, bytes) = alloc_guard::disarm();

    assert_eq!(allocs, 0, "matching allocated {allocs} times ({bytes} bytes)");
}

#[test]
fn queries_and_depth_snapshots_allocate_nothing() {
    let mut ob = book(4_000);
    let mut trades = TradeBuf::with_capacity(64);
    for i in 0..1_000u64 {
        trades.clear();
        ob.submit(
            &NewOrder::limit(ClientOrderId(i), Side::Buy, (i % 500) as i64, 1),
            &mut trades,
            TS,
        )
        .unwrap();
    }
    let mut depth = [LevelInfo { price: 0, volume: 0, order_count: 0 }; 20];

    alloc_guard::arm();
    let mut acc = 0i64;
    for _ in 0..100_000 {
        acc += ob.get_best_bid().unwrap_or(0);
        acc += ob.get_best_ask().unwrap_or(0);
        acc += ob.bid_depth(&mut depth) as i64;
    }
    let (allocs, bytes) = alloc_guard::disarm();
    std::hint::black_box(acc);

    assert_eq!(allocs, 0, "queries allocated {allocs} times ({bytes} bytes)");
}

#[test]
fn rejects_allocate_nothing() {
    let mut ob = book(4);
    let mut trades = TradeBuf::with_capacity(8);

    alloc_guard::arm();
    for i in 0..10_000u64 {
        trades.clear();
        // 出 band、零數量、arena 滿 —— 三條 reject 路徑
        let _ = ob.submit(&NewOrder::limit(ClientOrderId(i), Side::Buy, 999_999, 1), &mut trades, TS);
        let _ = ob.submit(&NewOrder::limit(ClientOrderId(i), Side::Buy, 100, 0), &mut trades, TS);
        let _ = ob.submit(&NewOrder::limit(ClientOrderId(i), Side::Buy, 100, 1), &mut trades, TS);
    }
    let (allocs, bytes) = alloc_guard::disarm();

    assert_eq!(allocs, 0, "reject path allocated {allocs} times ({bytes} bytes)");
}

/// 守住 alloc_guard 自己：counter 一定要係 thread-local。
///
/// 如果將來有人把佢改返做全域 `AtomicU64`，呢個 test 會紅 ——
/// 因為子 thread 嘅 allocation 會漏入主 thread 個數。
///
/// 時序好講究：
///   * `arm()` 一定要喺 `spawn()` **之後** —— `thread::spawn` 會喺
///     **呼叫者嗰條 thread** allocate（closure box、JoinHandle、內部 Arc…）
///   * `disarm()` 一定要喺 `join()` **之前** —— 同上道理
///   * 中間用 atomic spin 做同步，唔用 channel：mpsc 內部有機會 allocate，
///     會污染度量
#[test]
fn counter_is_isolated_per_thread() {
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::sync::Arc;

    const WAIT: u8 = 0;
    const GO: u8 = 1;
    const DONE: u8 = 2;

    // 呢兩個 allocation 喺 arm() 之前發生，唔會入數
    let state = Arc::new(AtomicU8::new(WAIT));
    let child_state = Arc::clone(&state);

    let handle = std::thread::spawn(move || {
        while child_state.load(Ordering::Acquire) != GO {
            std::hint::spin_loop();
        }
        // 喺子 thread 上大量 allocate
        let mut sink: Vec<Vec<u64>> = Vec::new();
        for i in 0..1_000u64 {
            sink.push(vec![i; 64]);
        }
        let n = sink.len();
        child_state.store(DONE, Ordering::Release);
        n
    });

    // 由呢度到 disarm()，主 thread 只做 atomic load/store 同 spin —— 零 allocation
    alloc_guard::arm();
    state.store(GO, Ordering::Release);
    while state.load(Ordering::Acquire) != DONE {
        std::hint::spin_loop();
    }
    let (allocs, bytes) = alloc_guard::disarm();

    let n = handle.join().unwrap();
    assert_eq!(n, 1_000);
    assert_eq!(
        allocs, 0,
        "子 thread 嘅 {allocs} 次 allocation ({bytes} bytes) 漏咗入主 thread 個數"
    );
}

/// 反向確認：guard 真係數到嘢，唔係永遠返 0。
#[test]
fn counter_actually_counts() {
    alloc_guard::arm();
    let v: Vec<u64> = Vec::with_capacity(1024);
    let z: Vec<u8> = vec![0; 4096]; // 行 alloc_zeroed
    let (allocs, bytes) = alloc_guard::disarm();
    std::hint::black_box((v, z));
    assert!(allocs >= 2, "expected at least 2 allocations, got {allocs}");
    assert!(bytes >= 4096 + 8192, "byte count too low: {bytes}");
}

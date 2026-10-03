//! Per-operation latency distribution.
//!
//! `benchmark.rs` 報嘅係 `total_time / n` —— 平均值恰恰係最睇唔到 tail 嘅指標。
//! 1,000,000 次入面有 100 次 5 µs 嘅 page fault，攤入平均值只係 +0.5 ns。
//!
//! 呢個 bin 收每一次操作嘅耗時，排序之後報 percentile。
//!
//! # 三個令數字可信嘅前提
//!
//! 1. **Workload 預先生成**：所有 `NewOrder` 同隨機數喺量度窗口**之前**
//!    砌好。喺 loop 入面 call RNG 會把 RNG 嘅耗時計埋落 engine 度。
//! 2. **Sample buffer 預先分配**：`Vec<u32>` 預留容量，量度期間唔 grow。
//!    排序喺 `disarm()` 之後先做。
//! 3. **`alloc_guard` 全程 arm 住**：如果量度窗口本身有 allocation，
//!    個 tail 就係量緊 allocator 而唔係 engine。每個 workload 都會報返
//!    allocation count，唔係 0 就要當啲數唔算數。
//!
//! # 一個要老實講嘅限制
//!
//! 呢個係 **closed loop**：上一個操作做完先發下一個。真實系統係 open loop
//! （流量按自己節奏到）。Closed loop 會低估 tail —— 系統卡住嗰陣，
//! 本應排隊嘅請求根本冇發出去（coordinated omission）。
//! 要準確量 tail under load，要用固定發送速率 + 記錄「應發時間」到完成嘅
//! 時間差。呢度嘅數字應該當成「無負載下嘅單次操作成本分佈」。

use std::hint::black_box;
use std::time::Instant;

use orderbook::alloc_guard;
use orderbook::{
    BookConfig, ClientOrderId, NewOrder, OrderBook, OrderId, OrderType, Side, TradeBuf,
};

#[global_allocator]
static ALLOC: alloc_guard::Counting = alloc_guard::Counting;

// ------------------------------------------------------------------ 工具

/// xorshift64 —— 確定性，冇外部依賴，benchmark 可重現。
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }
    #[inline]
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    #[inline]
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn commas(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out.chars().rev().collect()
}

fn ns(v: u32) -> String {
    if v < 1_000 {
        format!("{v} ns")
    } else if v < 1_000_000 {
        format!("{:.2} us", v as f64 / 1_000.0)
    } else {
        format!("{:.2} ms", v as f64 / 1_000_000.0)
    }
}

#[inline]
fn pct(sorted: &[u32], q: f64) -> u32 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * q).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// `Instant::now()` + `elapsed()` 自己嘅成本。每個 sample 都含住呢個數。
fn calibrate(iters: usize) -> u32 {
    let mut v = vec![0u32; iters];
    for s in v.iter_mut() {
        let t = Instant::now();
        *s = t.elapsed().as_nanos() as u32;
    }
    v.sort_unstable();
    v[iters / 2]
}

fn report(label: &str, samples: &mut [u32], allocs: u64, bytes: u64, overhead: u32) {
    samples.sort_unstable();
    let n = samples.len();
    let sum: u64 = samples.iter().map(|&x| x as u64).sum();
    let over_1us = samples.iter().filter(|&&x| x >= 1_000).count();
    let over_10us = samples.iter().filter(|&&x| x >= 10_000).count();

    println!("{label}   n={}", commas(n as u64));
    println!("  mean     {:>10}", ns((sum / n as u64) as u32));
    println!("  p50      {:>10}", ns(pct(samples, 0.50)));
    println!("  p90      {:>10}", ns(pct(samples, 0.90)));
    println!("  p99      {:>10}", ns(pct(samples, 0.99)));
    println!("  p99.9    {:>10}", ns(pct(samples, 0.999)));
    println!("  p99.99   {:>10}", ns(pct(samples, 0.9999)));
    println!("  max      {:>10}", ns(samples[n - 1]));
    println!(
        "  >=1us    {:>10}  ({:.4}%)",
        commas(over_1us as u64),
        over_1us as f64 * 100.0 / n as f64
    );
    println!(
        "  >=10us   {:>10}  ({:.4}%)",
        commas(over_10us as u64),
        over_10us as f64 * 100.0 / n as f64
    );
    if allocs == 0 {
        println!("  allocs   {:>10}", 0);
    } else {
        println!(
            "  allocs   {:>10}  ({} bytes)  <-- 量度窗口有 allocation，數字唔算數",
            commas(allocs),
            commas(bytes)
        );
    }
    println!(
        "  (timer overhead ~{} 已包含喺每個 sample 入面)\n",
        ns(overhead)
    );
}

// ------------------------------------------------------------- workloads

fn cfg(max_orders: usize, num_ticks: usize) -> BookConfig {
    BookConfig {
        max_orders,
        base_price: 0,
        tick_size: 1,
        num_ticks,
    }
}

/// 掛單，全部落喺同一個價位（最淺嘅 book）。
fn add_shallow(n: usize, overhead: u32) {
    let mut book = OrderBook::with_config(cfg(n + 16, 4096));
    book.warm_up();
    let mut trades = TradeBuf::default();

    // workload 預先生成
    let plan: Vec<NewOrder> = (0..n)
        .map(|i| NewOrder::limit(ClientOrderId(i as u64), Side::Buy, 100, 10))
        .collect();
    let mut samples: Vec<u32> = Vec::with_capacity(n);

    alloc_guard::arm();
    for req in &plan {
        trades.clear();
        let t = Instant::now();
        let r = black_box(&mut book).submit(req, black_box(&mut trades), 0);
        samples.push(t.elapsed().as_nanos() as u32);
        black_box(r).unwrap();
    }
    let (a, b) = alloc_guard::disarm();
    report("Add   (shallow: 1 level)", &mut samples, a, b, overhead);
}

/// 掛單散落 `levels` 個價位 —— 貼近真實 crypto perp book 嘅形狀。
///
/// `warm` 控制係咪叫 `warm_up()`。**預期兩者分別好細**，原因值得知：
/// `OrderArena::with_capacity` 入面個 `vec![OrderSlot::EMPTY; n]` 因為
/// `EMPTY` 唔係全零（`generation: 1`、`NIL` 都係非零），所以行嘅係
/// 「alloc + 逐個元素寫入」而唔係 `alloc_zeroed`，本身已經 touch 晒每一頁。
/// 之後個 free-list 串接 loop 再 touch 多次。
///
/// 即係話 `warm_up()` 嘅 pre-fault 對呢個 arena 其實**多數係冗餘**。
/// 佢仲有價值嘅部分係 `mlock`（防止之後被 swap 出去）同 Linux 嘅
/// `MADV_HUGEPAGE`。呢個 A/B 就係用嚟證實呢件事，唔係用嚟宣傳 warm_up。
fn add_deep(n: usize, levels: u32, warm: bool, overhead: u32) {
    let mut book = OrderBook::with_config(cfg(n + 16, (levels * 2) as usize));
    if warm {
        book.warm_up();
    }
    let mut trades = TradeBuf::default();

    let mut rng = Rng::new(0xC0FFEE);
    let plan: Vec<NewOrder> = (0..n)
        .map(|i| {
            NewOrder::limit(
                ClientOrderId(i as u64),
                Side::Buy,
                rng.below(levels as u64) as i64,
                10,
            )
        })
        .collect();
    let mut samples: Vec<u32> = Vec::with_capacity(n);

    alloc_guard::arm();
    for req in &plan {
        trades.clear();
        let t = Instant::now();
        let r = black_box(&mut book).submit(req, black_box(&mut trades), 0);
        samples.push(t.elapsed().as_nanos() as u32);
        black_box(r).unwrap();
    }
    let (a, b) = alloc_guard::disarm();

    let label = if warm {
        format!("Add   (deep: {levels} levels, warm_up ON )")
    } else {
        format!("Add   (deep: {levels} levels, warm_up OFF)")
    };
    report(&label, &mut samples, a, b, overhead);
}

/// 隨機次序 cancel —— 最壞嘅 cache 存取模式。
fn cancel_random(n: usize, levels: u32, overhead: u32) {
    let mut book = OrderBook::with_config(cfg(n + 16, (levels * 2) as usize));
    book.warm_up();
    let mut trades = TradeBuf::default();

    let mut rng = Rng::new(0xBEEF);
    let mut ids: Vec<OrderId> = Vec::with_capacity(n);
    for i in 0..n {
        trades.clear();
        let req = NewOrder::limit(
            ClientOrderId(i as u64),
            Side::Buy,
            rng.below(levels as u64) as i64,
            10,
        );
        ids.push(book.submit(&req, &mut trades, 0).unwrap());
    }

    // Fisher-Yates 打亂 —— 喺量度窗口之前做
    for i in (1..n).rev() {
        let j = rng.below((i + 1) as u64) as usize;
        ids.swap(i, j);
    }
    let mut samples: Vec<u32> = Vec::with_capacity(n);

    alloc_guard::arm();
    for &id in &ids {
        let t = Instant::now();
        let r = black_box(&mut book).cancel(id);
        samples.push(t.elapsed().as_nanos() as u32);
        black_box(r).unwrap();
    }
    let (a, b) = alloc_guard::disarm();
    report(
        "Cancel (random order, deep book)",
        &mut samples,
        a,
        b,
        overhead,
    );
}

/// 成交：book 掛滿多個價位，然後打入會 cross 嘅單。
fn match_deep(takers: usize, levels: u32, overhead: u32) {
    let makers = takers * 2;
    let mut book = OrderBook::with_config(cfg(makers + 64, (levels * 2) as usize));
    book.warm_up();
    let mut trades = TradeBuf::default();

    let mut rng = Rng::new(0x1234_5678);
    for i in 0..makers {
        trades.clear();
        let req = NewOrder::limit(
            ClientOrderId(i as u64),
            Side::Buy,
            rng.below(levels as u64) as i64,
            50,
        );
        book.submit(&req, &mut trades, 0).unwrap();
    }

    // Sell limit @ 0 -> cross 晒所有 bid，由 best 開始向下食
    let plan: Vec<NewOrder> = (0..takers)
        .map(|i| NewOrder {
            client_order_id: ClientOrderId((makers + i) as u64),
            order_type: OrderType::ImmediateOrCancel,
            side: Side::Sell,
            price: 0,
            quantity: 1 + rng.below(150),
        })
        .collect();
    let mut samples: Vec<u32> = Vec::with_capacity(takers);
    let mut fills = 0u64;

    alloc_guard::arm();
    for req in &plan {
        trades.clear();
        let t = Instant::now();
        let r = black_box(&mut book).submit(req, black_box(&mut trades), 0);
        samples.push(t.elapsed().as_nanos() as u32);
        fills += trades.len() as u64;
        black_box(r).unwrap();
    }
    let (a, b) = alloc_guard::disarm();
    report(
        &format!("Match (deep: {levels} levels)"),
        &mut samples,
        a,
        b,
        overhead,
    );
    println!(
        "  總成交筆數 {} ({:.2} 筆/單)\n",
        commas(fills),
        fills as f64 / takers as f64
    );
}

/// 最貼近生產嘅 shape：60% 掛單 / 30% 撤單 / 10% 市價單。
fn mixed(n: usize, levels: u32, overhead: u32) {
    let cap = n + 16;
    let mut book = OrderBook::with_config(cfg(cap, (levels * 2) as usize));
    book.warm_up();
    let mut trades = TradeBuf::default();

    #[derive(Clone, Copy)]
    enum Op {
        Add(NewOrder),
        Cancel(u32),
        Market(NewOrder),
    }

    let mut rng = Rng::new(0xDEAD_BEEF);
    let plan: Vec<Op> = (0..n)
        .map(|i| {
            let roll = rng.below(100);
            let price = rng.below(levels as u64) as i64;
            let cid = ClientOrderId(i as u64);
            if roll < 60 {
                Op::Add(NewOrder::limit(cid, Side::Buy, price, 10))
            } else if roll < 90 {
                Op::Cancel(rng.next() as u32)
            } else {
                Op::Market(NewOrder::market(cid, Side::Sell, 1 + rng.below(30)))
            }
        })
        .collect();

    // live id ring：預先分配，量度期間唔會 grow
    let mut live: Vec<OrderId> = Vec::with_capacity(cap);
    let mut samples: Vec<u32> = Vec::with_capacity(n);
    let mut stale_cancels: Vec<u32> = Vec::with_capacity(n);

    alloc_guard::arm();
    for &op in &plan {
        trades.clear();
        match op {
            Op::Add(req) => {
                let t = Instant::now();
                let r = black_box(&mut book).submit(&req, black_box(&mut trades), 0);
                samples.push(t.elapsed().as_nanos() as u32);
                let id = r.unwrap();
                if book.get(id).is_some() {
                    live.push(id);
                }
            }
            Op::Cancel(r) => {
                if live.is_empty() {
                    continue; // 冇單可撤 —— 唔記錄，否則會用 0 拉低分佈
                }
                let k = (r as usize) % live.len();
                let id = live.swap_remove(k);
                let t = Instant::now();
                let res = black_box(&mut book).cancel(id);
                let elapsed = t.elapsed().as_nanos() as u32;
                match res {
                    Ok(()) => samples.push(elapsed),
                    Err(orderbook::OrderBookError::OrderNotFound { .. }) => {
                        stale_cancels.push(elapsed)
                    }
                    Err(e) => panic!("unexpected cancel failure: {e}"),
                }
            }
            Op::Market(req) => {
                let t = Instant::now();
                let r = black_box(&mut book).submit(&req, black_box(&mut trades), 0);
                samples.push(t.elapsed().as_nanos() as u32);
                black_box(r).unwrap();
            }
        }
    }
    let (a, b) = alloc_guard::disarm();
    report(
        &format!("Mixed (planned 60/30/10; successful operations, {levels} levels)"),
        &mut samples,
        a,
        b,
        overhead,
    );
    if !stale_cancels.is_empty() {
        report(
            "Mixed stale-ID rejects (excluded above)",
            &mut stale_cancels,
            a,
            b,
            overhead,
        );
    }
    println!(
        "  收工時仲掛住 {} 張單\n",
        commas(book.live_orders() as u64)
    );
}

/// Separate priority-preserving reductions, increases and reprices.
fn amend(n: usize, overhead: u32) {
    for (label, price, qty) in [
        ("Reduce", 100, 5),
        ("Increase", 100, 20),
        ("Reprice", 101, 10),
    ] {
        let mut book = OrderBook::with_config(cfg(n + 16, 4096));
        let mut trades = TradeBuf::default();
        let ids: Vec<_> = (0..n)
            .map(|i| {
                book.submit(
                    &NewOrder::limit(ClientOrderId(i as u64), Side::Buy, 100, 10),
                    &mut trades,
                    0,
                )
                .unwrap()
            })
            .collect();
        let mut samples = Vec::with_capacity(n);
        alloc_guard::arm();
        for id in ids {
            let t = Instant::now();
            let result = black_box(&mut book).amend(id, price, qty, black_box(&mut trades), 0);
            samples.push(t.elapsed().as_nanos() as u32);
            black_box(result).unwrap();
        }
        let (a, b) = alloc_guard::disarm();
        report(label, &mut samples, a, b, overhead);
    }
}

/// One order per tick isolates occupancy transitions from existing-level updates.
fn level_transitions(n: usize, overhead: u32) {
    let mut book = OrderBook::with_config(cfg(n + 16, n));
    let mut trades = TradeBuf::default();
    let mut ids = Vec::with_capacity(n);
    let mut samples = Vec::with_capacity(n);
    alloc_guard::arm();
    for i in 0..n {
        let req = NewOrder::limit(ClientOrderId(i as u64), Side::Buy, i as i64, 1);
        let t = Instant::now();
        let result = black_box(&mut book).submit(&req, black_box(&mut trades), 0);
        samples.push(t.elapsed().as_nanos() as u32);
        ids.push(result.unwrap());
    }
    let (a, b) = alloc_guard::disarm();
    report("Add (new price level)", &mut samples, a, b, overhead);
    samples.clear();
    alloc_guard::arm();
    for id in ids {
        let t = Instant::now();
        let result = black_box(&mut book).cancel(id);
        samples.push(t.elapsed().as_nanos() as u32);
        black_box(result).unwrap();
    }
    let (a, b) = alloc_guard::disarm();
    report("Cancel (last order at level)", &mut samples, a, b, overhead);
}

// ------------------------------------------------------------------ main

fn main() {
    let n: usize = 1_000_000;

    let overhead = calibrate(100_000);
    println!("=================================================================");
    println!(
        " Latency distribution   ({} ops per workload)",
        commas(n as u64)
    );
    println!(
        " Timer overhead (median Instant::now + elapsed): {}",
        ns(overhead)
    );
    println!(
        " Samples include timer overhead; do not subtract percentiles as an exact correction."
    );
    println!("=================================================================\n");

    add_shallow(n, overhead);
    add_deep(n, 2_000, true, overhead);
    add_deep(n, 2_000, false, overhead);
    cancel_random(n, 2_000, overhead);
    match_deep(n / 2, 2_000, overhead);
    mixed(n, 2_000, overhead);
    amend(n, overhead);
    level_transitions(65_536, overhead);

    println!("提示：測量前把機器靜落嚟（關 Spotlight indexing、瀏覽器），");
    println!("      macOS 上仲可以用 `sudo nice -n -20` 減少被搶佔。");
}

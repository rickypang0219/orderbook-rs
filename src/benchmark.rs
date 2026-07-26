use std::time::{Instant, SystemTime, UNIX_EPOCH};

use rand::distributions::Uniform;
use rand::prelude::*;

use orderbook::alloc_guard;
use orderbook::{BookConfig, ClientOrderId, NewOrder, OrderBook, OrderId, Side, TradeBuf};

#[global_allocator]
static ALLOC: alloc_guard::Counting = alloc_guard::Counting;

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
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

fn report(label: &str, n: u64, d: std::time::Duration, allocs: u64, bytes: u64) {
    let secs = d.as_secs_f64();
    let ops = if secs > 0.0 { (n as f64 / secs) as u64 } else { 0 };
    println!("{label} {}:", commas(n));
    println!("  Time:        {:.2} ms", d.as_micros() as f64 / 1000.0);
    println!("  Throughput:  {} ops/sec", commas(ops));
    println!("  Latency:     {:.3} us/op", d.as_micros() as f64 / n as f64);
    println!(
        "  Allocations: {} ({:.4}/op, {} bytes)\n",
        commas(allocs),
        allocs as f64 / n as f64,
        commas(bytes)
    );
}

fn fresh(n: u64) -> OrderBook {
    let mut b = OrderBook::with_config(BookConfig {
        max_orders: n as usize + 16,
        base_price: 0,
        tick_size: 1,
        num_ticks: 4096,
    });
    b.warm_up();
    b
}

fn bench_add(n: u64) {
    let mut book = fresh(n);
    let mut trades = TradeBuf::default();
    let mut rng = thread_rng();
    let (price, qty, side) = (
        Uniform::new_inclusive(90, 110),
        Uniform::new_inclusive(1, 100),
        Uniform::new_inclusive(0, 1),
    );
    let ts = now_ns();

    alloc_guard::arm();
    let start = Instant::now();
    for i in 0..n {
        let req = NewOrder::limit(
            ClientOrderId(i),
            if side.sample(&mut rng) == 1 { Side::Buy } else { Side::Sell },
            price.sample(&mut rng),
            qty.sample(&mut rng),
        );
        trades.clear();
        let _ = book.submit(&req, &mut trades, ts);
    }
    let d = start.elapsed();
    let (a, b) = alloc_guard::disarm();
    report("Add", n, d, a, b);
}

fn bench_cancel(n: u64) {
    let mut book = fresh(n);
    let mut trades = TradeBuf::default();
    let mut ids: Vec<OrderId> = Vec::with_capacity(n as usize);
    let ts = now_ns();
    for i in 0..n {
        trades.clear();
        ids.push(
            book.submit(&NewOrder::limit(ClientOrderId(i), Side::Buy, 100, 10), &mut trades, ts)
                .unwrap(),
        );
    }

    alloc_guard::arm();
    let start = Instant::now();
    for id in &ids {
        book.cancel(*id).unwrap();
    }
    let d = start.elapsed();
    let (a, b) = alloc_guard::disarm();
    report("Cancel", n, d, a, b);
}

fn bench_match(n: u64) {
    let mut book = fresh(n);
    let mut trades = TradeBuf::default();
    let mut rng = thread_rng();
    let qty = Uniform::new_inclusive(1, 100);
    let ts = now_ns();

    for i in 0..n / 2 {
        trades.clear();
        let req = NewOrder::limit(ClientOrderId(i), Side::Buy, 100, qty.sample(&mut rng));
        book.submit(&req, &mut trades, ts).unwrap();
    }

    let mut executed = 0u64;
    alloc_guard::arm();
    let start = Instant::now();
    for i in n / 2..n {
        let req = NewOrder::limit(ClientOrderId(i), Side::Sell, 100, qty.sample(&mut rng));
        trades.clear();
        let _ = book.submit(&req, &mut trades, ts);
        executed += trades.len() as u64;
    }
    let d = start.elapsed();
    let (a, b) = alloc_guard::disarm();
    report("Match", n / 2, d, a, b);
    println!("  Trades executed: {}\n", commas(executed));
}

fn bench_best_price(n: u64) {
    let mut book = fresh(n);
    let mut trades = TradeBuf::default();
    let ts = now_ns();
    for i in 0..2000u64 {
        trades.clear();
        book.submit(&NewOrder::limit(ClientOrderId(i), Side::Buy, (i % 2000) as i64, 1), &mut trades, ts)
            .unwrap();
    }

    alloc_guard::arm();
    let start = Instant::now();
    let mut acc = 0i64;
    for _ in 0..n {
        acc += book.get_best_bid().unwrap_or(0);
    }
    let d = start.elapsed();
    let (a, b) = alloc_guard::disarm();
    std::hint::black_box(acc);
    report("BestBid", n, d, a, b);
}

fn main() {
    let n: u64 = 1_000_000;
    bench_add(n);
    bench_cancel(n);
    bench_match(n);
    bench_best_price(n);
}

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use rand::distributions::Uniform;
use rand::prelude::*;

pub mod alloc_guard;
pub mod orderbook;

#[global_allocator]
static ALLOC: alloc_guard::Counting = alloc_guard::Counting;

use orderbook::order::{NewOrder, Side};
use orderbook::orderbook_impl::{BookConfig, OrderBook, TradeBuf};
use orderbook::types::{ClientOrderId, OrderId};

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

fn format_number(n: u64) -> String {
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
    let ops = if secs > 0.0 {
        (n as f64 / secs) as u64
    } else {
        0
    };
    println!("{label} {}:", format_number(n));
    println!("  Time: {:.2} ms", d.as_micros() as f64 / 1000.0);
    println!("  Throughput: {} ops/sec", format_number(ops));
    println!("  Latency: {:.3} us/op", d.as_micros() as f64 / n as f64);
    println!(
        "  Allocations: {} ({:.4} per op, {} bytes)\n",
        format_number(allocs),
        allocs as f64 / n as f64,
        format_number(bytes)
    );
}

fn cfg(n: u64) -> BookConfig {
    BookConfig {
        max_orders: (n as usize) + 16,
        max_levels: 4096,
    }
}

fn benchmark_add_orders(n: u64) {
    let mut book = OrderBook::with_config(cfg(n));
    let mut trades = TradeBuf::default();
    let mut rng = thread_rng();
    let price = Uniform::new_inclusive(90, 110);
    let qty = Uniform::new_inclusive(1, 100);
    let side = Uniform::new_inclusive(0, 1);
    let ts = now_ns();

    alloc_guard::arm();
    let start = Instant::now();
    for i in 0..n {
        let req = NewOrder::limit(
            ClientOrderId(i),
            if side.sample(&mut rng) == 1 {
                Side::Buy
            } else {
                Side::Sell
            },
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

fn benchmark_cancel_orders(n: u64) {
    let mut book = OrderBook::with_config(cfg(n));
    let mut trades = TradeBuf::default();
    let mut ids: Vec<OrderId> = Vec::with_capacity(n as usize);
    let ts = now_ns();

    for i in 0..n {
        trades.clear();
        let req = NewOrder::limit(ClientOrderId(i), Side::Buy, 100, 10);
        ids.push(book.submit(&req, &mut trades, ts).unwrap());
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

fn benchmark_match_orders(n: u64) {
    let mut book = OrderBook::with_config(cfg(n));
    let mut trades = TradeBuf::default();
    let mut rng = thread_rng();
    let qty = Uniform::new_inclusive(1, 100);
    let ts = now_ns();

    for i in 0..n / 2 {
        trades.clear();
        let req = NewOrder::limit(ClientOrderId(i), Side::Buy, 100, qty.sample(&mut rng));
        book.submit(&req, &mut trades, ts).unwrap();
    }

    let mut executed: u64 = 0;
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
    println!("  Trades executed: {}\n", format_number(executed));
}

fn main() {
    let n: u64 = 1_000_000;
    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Warn)
        .init();

    benchmark_add_orders(n);
    benchmark_cancel_orders(n);
    benchmark_match_orders(n);
}

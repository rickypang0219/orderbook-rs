use std::hint::black_box;
use std::sync::Arc;
use std::time::Duration;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use orderbook::orderbook::order::{Order, OrderType, Side};
use orderbook::orderbook::orderbook_impl::OrderBook;
use uuid::Uuid;

const BATCH_SIZE: usize = 10_000;

fn order(id: u128, side: Side, price: i64, quantity: u64) -> Arc<Order> {
    Arc::new(Order::with_id(
        Uuid::from_u128(id),
        OrderType::GoodTillCancel,
        side,
        price,
        quantity,
    ))
}

fn resting_orders(id_offset: u128) -> Vec<Arc<Order>> {
    (0..BATCH_SIZE)
        .map(|i| order(id_offset + i as u128, Side::Buy, 90 + (i % 21) as i64, 10))
        .collect()
}

fn benchmark_add(c: &mut Criterion) {
    let mut group = c.benchmark_group("add_resting_orders");
    group.throughput(Throughput::Elements(BATCH_SIZE as u64));

    group.bench_function(BenchmarkId::new("default_capacity", BATCH_SIZE), |b| {
        b.iter_batched(
            || (OrderBook::new(), resting_orders(1)),
            |(mut book, orders)| {
                for order in &orders {
                    black_box(book.add_order(order).unwrap());
                }
                black_box(book)
            },
            BatchSize::LargeInput,
        )
    });

    group.bench_function(BenchmarkId::new("preallocated", BATCH_SIZE), |b| {
        b.iter_batched(
            || (OrderBook::with_capacity(BATCH_SIZE, 32), resting_orders(1)),
            |(mut book, orders)| {
                for order in &orders {
                    black_box(book.add_order(order).unwrap());
                }
                black_box(book)
            },
            BatchSize::LargeInput,
        )
    });
    group.finish();
}

fn benchmark_cancel(c: &mut Criterion) {
    let mut group = c.benchmark_group("cancel_orders");
    group.throughput(Throughput::Elements(BATCH_SIZE as u64));
    group.bench_function(BenchmarkId::from_parameter(BATCH_SIZE), |b| {
        b.iter_batched(
            || {
                let orders = resting_orders(1);
                let ids = orders
                    .iter()
                    .map(|order| order.order_id)
                    .collect::<Vec<_>>();
                let mut book = OrderBook::with_capacity(BATCH_SIZE, 32);
                for order in &orders {
                    book.add_order(order).unwrap();
                }
                (book, ids)
            },
            |(mut book, ids)| {
                for id in ids {
                    book.cancel_order(black_box(id)).unwrap();
                }
                black_box(book)
            },
            BatchSize::LargeInput,
        )
    });
    group.finish();
}

fn benchmark_match(c: &mut Criterion) {
    let mut group = c.benchmark_group("match_orders");
    group.throughput(Throughput::Elements(BATCH_SIZE as u64));
    group.bench_function(BenchmarkId::from_parameter(BATCH_SIZE), |b| {
        b.iter_batched(
            || {
                let resting = (0..BATCH_SIZE)
                    .map(|i| order(1 + i as u128, Side::Buy, 100, 10))
                    .collect::<Vec<_>>();
                let incoming = (0..BATCH_SIZE)
                    .map(|i| order(1_000_001 + i as u128, Side::Sell, 100, 10))
                    .collect::<Vec<_>>();
                let mut book = OrderBook::with_capacity(BATCH_SIZE, 1);
                for order in &resting {
                    book.add_order(order).unwrap();
                }
                (book, incoming)
            },
            |(mut book, incoming)| {
                for order in &incoming {
                    black_box(book.add_order(order).unwrap());
                }
                black_box(book)
            },
            BatchSize::LargeInput,
        )
    });
    group.finish();
}

fn criterion_config() -> Criterion {
    Criterion::default()
        .sample_size(20)
        .measurement_time(Duration::from_secs(5))
}

criterion_group! {
    name = benches;
    config = criterion_config();
    targets = benchmark_add, benchmark_cancel, benchmark_match
}
criterion_main!(benches);

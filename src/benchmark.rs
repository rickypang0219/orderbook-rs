use env_logger;
use log::LevelFilter;
use std::sync::Arc;
use std::time::Instant;

use rand::distributions::Uniform;
use rand::prelude::*;
use uuid::Uuid;

pub mod orderbook;

use orderbook::order::{Order, OrderType, Side};
use orderbook::orderbook_impl::OrderBook;

fn format_number(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::new();
    let chars: Vec<char> = s.chars().rev().collect();
    for (i, c) in chars.iter().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(*c);
    }
    result.chars().rev().collect()
}

fn benchmark_add_orders(num_orders: u64) {
    let mut orderbook = OrderBook::new(4096, 4096);

    // Set up random number generator
    let mut rng = thread_rng();
    let price_dist_bid = Uniform::new_inclusive(90, 110); // Price range [90, 110]
    let price_dist_ask = Uniform::new_inclusive(111, 120);
    let qty_dist = Uniform::new_inclusive(1, 100); // Quantity range [1, 100]
    let side_dist = Uniform::new_inclusive(0, 1); // Side: 0 (Sell) or 1 (Buy)

    // Measure time for adding orders
    let start = Instant::now();

    // Add random orders to the book
    for _i in 0..num_orders {
        let side = if side_dist.sample(&mut rng) == 1 {
            Side::Buy
        } else {
            Side::Sell
        };
        let price_dist = if side == Side::Buy {
            price_dist_bid
        } else {
            price_dist_ask
        };
        let order = Arc::new(Order::new(
            OrderType::GoodTillCancel,
            side,
            price_dist.sample(&mut rng), // Random price
            qty_dist.sample(&mut rng),   // Random quantity
        ));
        orderbook.handle_order(&order).unwrap(); // Adjust error handling as needed
    }

    let duration = start.elapsed();

    let seconds = duration.as_secs_f64();
    let orders_per_sec = if seconds > 0.0 {
        (num_orders as f64 / seconds) as u64
    } else {
        0
    };
    let latency_us = duration.as_micros() as f64 / num_orders as f64;

    // Print results
    println!("Add {} orders:", format_number(num_orders));
    println!("  Time: {:.2} ms", duration.as_micros() as f64 / 1000.0);
    println!("  Throughput: {} orders/sec", format_number(orders_per_sec));
    println!("  Latency: {:.3} μs/order\n", latency_us);
}

fn benchmark_cancel_orders(num_orders: u64) {
    let mut orderbook = OrderBook::new(1024, 1024);
    let mut order_ids: Vec<Uuid> = Vec::with_capacity(num_orders as usize);

    // Add orders to the book
    for _i in 0..num_orders {
        let order = Arc::new(Order::new(OrderType::GoodTillCancel, Side::Buy, 100, 10));
        orderbook.handle_order(&order).unwrap();
        order_ids.push(order.order_id);
    }

    let start = Instant::now();

    // Cancel all orders
    for order_id in &order_ids {
        orderbook.cancel_order(*order_id).unwrap();
    }

    let duration = start.elapsed();

    let seconds = duration.as_secs_f64();
    let cancels_per_sec = if seconds > 0.0 {
        (num_orders as f64 / seconds) as u64
    } else {
        0
    };
    let latency_us = duration.as_micros() as f64 / num_orders as f64;

    println!("Cancel {} orders:", format_number(num_orders));
    println!("  Time: {:.2} ms", duration.as_micros() as f64 / 1000.0);
    println!(
        "  Throughput: {} cancels/sec",
        format_number(cancels_per_sec)
    );
    println!("  Latency: {:.3} μs/cancel\n", latency_us);
}

fn benchmark_match_orders(num_orders: u64) {
    let mut orderbook = OrderBook::new(1024, 1024);

    // Set up random number generator for quantities
    let mut rng = thread_rng();
    let qty_dist = Uniform::new_inclusive(1, 100); // Quantity range [1, 100]

    // Fill one side of the book with buy orders
    for _ in 0..num_orders / 2 {
        let order = Arc::new(Order::new(
            OrderType::GoodTillCancel,
            Side::Buy,
            100,                       // Fixed price
            qty_dist.sample(&mut rng), // Random quantity
        ));
        orderbook.handle_order(&order).unwrap(); // Assume no matching for buy orders
    }

    let mut trades_executed: u64 = 0;
    let start = Instant::now();

    // Add matching sell orders and measure matching speed
    for _ in num_orders / 2..num_orders {
        let order = Arc::new(Order::new(
            OrderType::GoodTillCancel,
            Side::Sell,
            100,                       // Fixed price to match buy orders
            qty_dist.sample(&mut rng), // Random quantity
        ));
        let trades = orderbook.handle_order(&order).unwrap();
        trades_executed += trades.len() as u64; // Count number of trades
    }

    let duration = start.elapsed();

    // Calculate metrics
    let seconds = duration.as_secs_f64(); // Duration in seconds
    let matches_per_sec = if seconds > 0.0 {
        ((num_orders / 2) as f64 / seconds) as u64
    } else {
        0
    };
    let trades_per_sec = if seconds > 0.0 {
        (trades_executed as f64 / seconds) as u64
    } else {
        0
    };

    // Print results
    println!("Match {} orders:", format_number(num_orders / 2));
    println!("  Time: {:.2} ms", duration.as_micros() as f64 / 1000.0);
    println!("  Trades executed: {}", format_number(trades_executed));
    println!(
        "  Throughput: {} matches/sec",
        format_number(matches_per_sec)
    );
    println!(
        "  Trade rate: {} trades/sec\n",
        format_number(trades_per_sec)
    );
}

fn main() {
    let num_orders: u64 = 100_000;

    env_logger::Builder::new()
        .filter_level(LevelFilter::Info)
        .init();

    benchmark_add_orders(num_orders);
    benchmark_cancel_orders(num_orders);
    benchmark_match_orders(num_orders);
}

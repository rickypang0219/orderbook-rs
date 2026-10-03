use std::time::{SystemTime, UNIX_EPOCH};

use orderbook::{ClientOrderId, NewOrder, OrderBook, Side, TradeBuf};

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

fn main() {
    // 所有 allocation 喺呢三行發生
    let mut book = OrderBook::new();
    let report = book.warm_up();
    let mut trades = TradeBuf::default();
    println!("warm-up: {report:?}");

    trades.clear();
    let maker_id = book
        .submit(
            &NewOrder::limit(ClientOrderId(1), Side::Buy, 10, 10),
            &mut trades,
            now_ns(),
        )
        .unwrap();
    println!("resting order {maker_id}");

    trades.clear();
    book.submit(
        &NewOrder::market(ClientOrderId(2), Side::Sell, 4),
        &mut trades,
        now_ns(),
    )
    .unwrap();
    println!("trades: {:?}", trades.as_slice());
    println!(
        "resting remaining: {:?}",
        book.get(maker_id).map(|s| s.remaining_qty)
    );
    println!("best bid: {:?}", book.get_best_bid());
}

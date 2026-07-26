use std::time::{SystemTime, UNIX_EPOCH};

pub mod orderbook;
use orderbook::order::{NewOrder, Side};
use orderbook::orderbook_impl::{OrderBook, TradeBuf};
use orderbook::types::ClientOrderId;

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

fn main() {
    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Info)
        .init();

    // 所有 allocation 喺呢兩行發生，之後 hot path 唔會再掂 malloc
    let mut book = OrderBook::new();
    let mut trades = TradeBuf::default();

    let maker = NewOrder::limit(ClientOrderId(1), Side::Buy, 10, 10);
    trades.clear();
    let maker_id = book.submit(&maker, &mut trades, now_ns()).unwrap();
    println!("resting order {maker_id}, trades: {:?}", trades.as_slice());

    let taker = NewOrder::market(ClientOrderId(2), Side::Sell, 4);
    trades.clear();
    book.submit(&taker, &mut trades, now_ns()).unwrap();
    println!("trades: {:?}", trades.as_slice());
    println!("resting remaining: {:?}", book.get(maker_id).map(|s| s.remaining_qty));
}

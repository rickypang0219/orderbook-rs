use std::sync::Arc;

pub mod orderbook;
use orderbook::order::{Order, OrderType, Side};
use orderbook::orderbook_impl::OrderBook;

fn main() {
    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Info)
        .init();

    let mut test_ob = OrderBook::new(1024, 1024);
    let limit_order = Arc::new(Order::new(OrderType::LimitOrder, Side::Buy, 10, 10));
    let trades = test_ob.handle_order(&limit_order).unwrap();
    println!("trades {:?}", trades);
}

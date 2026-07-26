use std::sync::Arc;

pub mod orderbook;
use orderbook::order::{Order, OrderType, Side};
use orderbook::orderbook_impl::{OrderBook, TradeBuf};

fn main() {
    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Info)
        .init();

    let mut test_ob = OrderBook::new();
    // TradeBuf 喺 init 分配一次，之後每個 loop 只係 clear()
    let mut trades = TradeBuf::default();

    let limit_order = Arc::new(Order::new(OrderType::LimitOrder, Side::Buy, 10, 10));
    trades.clear();
    test_ob.add_order(&limit_order, &mut trades).unwrap();
    println!("trades {:?}", trades.as_slice());
}

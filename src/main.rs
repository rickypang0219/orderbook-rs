use std::sync::Arc;

pub mod orderbook;
use orderbook::order::{Order, OrderType, Side};
use orderbook::orderbook_impl::OrderBook;

fn main() {
    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Info)
        .init();
    let mut test_ob = OrderBook::new();
    let market_order = Arc::new(Order::new(OrderType::MarketOrder, Side::Sell, 10, 10));

    // limit order first arrives to the OB
    {
        let limit_order_1 = Arc::new(Order::new(OrderType::LimitOrder, Side::Buy, 10, 10));
        let limit_order_2 = Arc::new(Order::new(OrderType::LimitOrder, Side::Buy, 11, 5));
        let limit_order_3 = Arc::new(Order::new(OrderType::LimitOrder, Side::Buy, 12, 3));

        test_ob.add_order(&limit_order_1).unwrap();
        test_ob.add_order(&limit_order_2).unwrap();
        test_ob.add_order(&limit_order_3).unwrap();
    }
    // Market Order arrives later to consume the OB
    let trades = test_ob.add_order(&market_order).unwrap();
}

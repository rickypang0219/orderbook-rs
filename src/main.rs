pub mod orderbook;
use orderbook::models::{LevelInfo, OrderbookLevelInfos};

fn main() {
    let bids = vec![LevelInfo::init_level_info(100, 10)];
    let asks = vec![LevelInfo::init_level_info(101, 5)];
    let orderbook = OrderbookLevelInfos::init_orderbook_levels(bids, asks);
    println!("bids {:?}", orderbook.get_bids());
    println!("asks {:?}", orderbook.get_asks());
}

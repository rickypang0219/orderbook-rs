pub type Price = i32;
pub type Quantity = u32;
pub type OrderId = u64;

pub enum OrderType {
    GoodTilCancel,
    ImmediateOrCancel,
    FillAndKill,
}

pub enum Side {
    Buy,
    Sell,
}

#[derive(Debug)]
pub struct LevelInfo {
    price: Price,
    qty: Quantity,
}

impl LevelInfo {
    pub fn init_level_info(price: Price, qty: Quantity) -> Self {
        LevelInfo { price, qty }
    }
}

pub type LevelInfos = Vec<LevelInfo>;

#[derive(Debug)]
pub struct OrderbookLevelInfos {
    bids: LevelInfos,
    asks: LevelInfos,
}

impl OrderbookLevelInfos {
    pub fn init_orderbook_levels(bids: LevelInfos, asks: LevelInfos) -> Self {
        OrderbookLevelInfos { bids, asks }
    }

    pub fn get_bids(&self) -> &LevelInfos {
        &self.bids
    }

    pub fn get_asks(&self) -> &LevelInfos {
        &self.asks
    }
}

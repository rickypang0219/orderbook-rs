use crate::orderbook::types::{ClientOrderId, Price, Quantity};

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
#[repr(u8)]
pub enum OrderType {
    LimitOrder,
    MarketOrder,
    ImmediateOrCancel,
    FillOrKill,
    GoodTillCancel,
}

impl OrderType {
    /// 未食完嘅餘數會唔會掛落 book？
    /// Market / IOC / FOK 都唔會 —— 原本 code 冇統一表達呢個概念，
    /// IOC 就係咁樣變成一個 `=> {}` 嘅空 arm。
    #[inline(always)]
    pub const fn rests_on_book(self) -> bool {
        matches!(self, OrderType::LimitOrder | OrderType::GoodTillCancel)
    }

    #[inline(always)]
    pub const fn is_market(self) -> bool {
        matches!(self, OrderType::MarketOrder)
    }
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
#[repr(u8)]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    #[inline(always)]
    pub const fn opposite(self) -> Side {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
#[repr(u8)]
pub enum Status {
    New,
    PartiallyFilled,
    Filled,
    Canceled,
}

/// 由外面入嚟嘅新單請求。
///
/// **冇 `order_id` field** —— 呢個係整個重構嘅語義核心。
/// Client 提供 `ClientOrderId`；`OrderId` 由 engine accept 之後先分配並返回。
/// 原本 `Order::new()` 自己 `Uuid::new_v4()` 出嚟嗰個 ID 兩者都唔係。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NewOrder {
    pub client_order_id: ClientOrderId,
    pub order_type: OrderType,
    pub side: Side,
    pub price: Price,
    pub quantity: Quantity,
}

impl NewOrder {
    pub const fn limit(
        client_order_id: ClientOrderId,
        side: Side,
        price: Price,
        quantity: Quantity,
    ) -> Self {
        NewOrder {
            client_order_id,
            order_type: OrderType::LimitOrder,
            side,
            price,
            quantity,
        }
    }

    pub const fn market(client_order_id: ClientOrderId, side: Side, quantity: Quantity) -> Self {
        NewOrder {
            client_order_id,
            order_type: OrderType::MarketOrder,
            side,
            price: 0,
            quantity,
        }
    }
}

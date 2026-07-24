use chrono::Utc;
use uuid::Uuid;

use crate::orderbook::custom_errors::QuantityError;
use crate::orderbook::types::{Price, Quantity};

#[derive(PartialEq, Clone, Copy, Debug)]
pub enum OrderType {
    LimitOrder,
    MarketOrder,
    ImmediateOrCancel,
    FillOrKill,
    GoodTillCancel,
}

#[derive(PartialEq, Clone, Copy, Debug)]
pub enum Side {
    Buy,
    Sell,
}

#[derive(PartialEq, Clone, Copy, Debug)]
pub enum Status {
    New,
    PartiallyFilled,
    Filled,
    Canceled,
}

#[derive(Debug, Clone)]
pub struct Order {
    pub order_type: OrderType,
    pub order_id: Uuid, // use uuid to replace u64
    pub side: Side,
    pub price: Price,
    pub status: Status,
    pub original_quantity: Quantity,
    pub executed_quantity: Quantity,
    pub remaining_quantity: Quantity,
    pub timestamp: i64,
}

impl Order {
    pub fn new(
        order_type: OrderType,
        side: Side,
        price: Price,
        original_quantity: Quantity,
    ) -> Self {
        Self::with_id(Uuid::new_v4(), order_type, side, price, original_quantity)
    }

    /// Constructs an order with a caller-supplied ID.
    ///
    /// This is useful for deterministic replay, tests, and benchmarks where UUID
    /// generation should not be included in the operation being measured.
    pub fn with_id(
        order_id: Uuid,
        order_type: OrderType,
        side: Side,
        price: Price,
        original_quantity: Quantity,
    ) -> Self {
        Order {
            order_type,
            order_id,
            side,
            price,
            status: Status::New,
            original_quantity,
            executed_quantity: 0,
            remaining_quantity: original_quantity,
            timestamp: Utc::now().timestamp_millis(),
        }
    }

    pub fn fill_qty(&mut self, quantity: Quantity) -> Result<(), QuantityError> {
        if (self.original_quantity - self.executed_quantity) < quantity {
            Err(QuantityError {
                message: format!(
                    "Quantity Error: remaining quantity {} ; fill quantity {}",
                    (self.original_quantity - self.executed_quantity),
                    quantity,
                ),
            })
        } else {
            self.executed_quantity += quantity;
            self.remaining_quantity = self.original_quantity - self.executed_quantity;
            Ok(())
        }
    }

    pub fn is_filled(&self) -> bool {
        self.remaining_quantity == 0
    }
}

#[cfg(test)]
mod order_tests {
    use super::*;

    #[test]
    fn check_new_order() {
        let test_order: Order = Order::new(OrderType::GoodTillCancel, Side::Buy, 100, 10);
        assert_eq!(test_order.price, 100);
        assert_eq!(test_order.order_type, OrderType::GoodTillCancel);
        assert_eq!(test_order.side, Side::Buy);
        assert_eq!(test_order.original_quantity, 10);
        assert_eq!(test_order.executed_quantity, 0);
    }

    #[test]
    fn check_fill_quantity() {
        let mut test_order: Order = Order::new(OrderType::GoodTillCancel, Side::Buy, 100, 10);
        let _ = test_order.fill_qty(10);
        assert_eq!(test_order.executed_quantity, 10);
        assert_eq!(test_order.remaining_quantity, 0);
        assert!(test_order.is_filled());
    }
}

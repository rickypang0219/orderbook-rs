use crate::orderbook::custom_errors::QuantityError;
use crate::orderbook::models::{OrderId, OrderType, Price, Quantity, Side};

pub struct Order {
    order_type: OrderType,
    order_id: OrderId,
    side: Side,
    price: Price,
    initial_quantity: Quantity,
    remaining_quantity: Quantity,
}

impl Order {
    pub fn init_order(
        order_type: OrderType,
        order_id: OrderId,
        side: Side,
        price: Price,
        initial_quantity: Quantity,
        remaining_quantity: Quantity,
    ) -> Self {
        Order {
            order_type,
            order_id,
            side,
            price,
            initial_quantity,
            remaining_quantity,
        }
    }

    pub fn get_side(&self) -> &Side {
        &self.side
    }

    pub fn get_order_type(&self) -> &OrderType {
        &self.order_type
    }

    pub fn get_price(&self) -> &Price {
        &self.price
    }

    pub fn get_order_id(&self) -> &OrderId {
        &self.order_id
    }

    pub fn get_init_qty(&self) -> &Quantity {
        &self.initial_quantity
    }

    pub fn get_remaining_qty(&self) -> &Quantity {
        &self.remaining_quantity
    }

    pub fn get_filled_qty(self) -> Quantity {
        *self.get_init_qty() - *self.get_remaining_qty()
    }

    pub fn fill_qty(&mut self, quantity: Quantity) -> Result<(), QuantityError> {
        if *self.get_remaining_qty() < quantity {
            Err(QuantityError {
                message: format!(
                    "Quantity Error: remaining quantity {} ; fill quantity {}",
                    self.remaining_quantity, quantity,
                ),
            })
        } else {
            self.remaining_quantity -= quantity;
            Ok(())
        }
    }
}

#[cfg(test)]
mod order_tests {
    use super::*;

    #[test]
    fn check_get_price() {
        let order: Order =
            Order::init_order(OrderType::GoodTilCancel, 1234567890, Side::Buy, 100, 10, 10);
        assert_eq!(*order.get_price(), 100);
    }

    #[test]
    fn check_init_qty() {
        let order: Order =
            Order::init_order(OrderType::GoodTilCancel, 1234567890, Side::Buy, 100, 10, 10);
        assert_eq!(*order.get_init_qty(), 10);
    }

    #[test]
    fn check_remain_qty() {
        let order: Order =
            Order::init_order(OrderType::GoodTilCancel, 1234567890, Side::Buy, 100, 10, 10);
        assert_eq!(*order.get_remaining_qty(), 10);
    }

    #[test]
    fn check_valid_fill_qty() {
        let mut order: Order =
            Order::init_order(OrderType::GoodTilCancel, 1234567890, Side::Buy, 100, 10, 10);
        assert_eq!(order.fill_qty(5).unwrap(), ());
    }

    #[test]
    fn check_invalid_fill_qty() {
        let mut order: Order =
            Order::init_order(OrderType::GoodTilCancel, 1234567890, Side::Buy, 100, 10, 10);
        assert!(order.fill_qty(11).is_err());
    }

    #[test]
    fn check_remain_qty_after_fill() {
        let mut order: Order =
            Order::init_order(OrderType::GoodTilCancel, 1234567890, Side::Buy, 100, 10, 10);
        let _fill5 = order.fill_qty(10);
        assert_eq!(*order.get_remaining_qty(), 0);
    }
}

use std::collections::VecDeque;
use std::sync::Arc;

use crate::orderbook::order::{Order, Side};
use crate::orderbook::types::{OrderId, Price, Quantity};

#[derive(Debug)]
pub struct PriceLevel {
    pub price: Price,
    pub orders: Vec<Option<OrderId>>,
    pub volume: Quantity,
    pub order_count: usize,
    pub avail_indices: VecDeque<usize>,
    pub capacity: usize,
}

#[derive(Debug)]
pub struct OrderEntry {
    pub order: Arc<Order>,
    pub order_cursor: usize,
    pub price_level_cursor: usize,
}

#[derive(Debug)]
pub struct ModifyOrder {
    pub order_id: OrderId,
    pub price: Price,
    pub side: Side,
    pub quantity: Quantity,
}

#[derive(Debug)]
pub struct LevelInfo {
    pub price: Price,
    pub volume: Quantity,
}

// Problem: get loss of active order from orders
// orders can contain None
// without tracking the active order position
// we will get None from accesing orderId from self.orders.get() or pop_front()

// suppose we have active_queue and free_indices as VecDeque
// when we consume resting order, look at the active_queue
// get the orderId by active_queue.get(0)
// use the orderId to query OrderEntry -> get resting Order
// when resting order is fully filled -> really pop_front and put pack the index to free_indices

// remove order
// O(1) access since we save the order_cursor from OrderEntry
// set the order to None using that cursor
// remove the cursor from active_queue ( CANNOT BE DONE IN O(1) )
// Normal case (not external operation), we can really remove cursor by pop_front
// external cancel

impl PriceLevel {
    pub fn new(price: Price, capacity: usize) -> Self {
        let avail_indices: VecDeque<usize> = VecDeque::with_capacity(capacity);
        let orders: Vec<Option<OrderId>> = Vec::with_capacity(capacity);

        Self {
            price,
            orders,
            volume: 0,
            order_count: 0,
            avail_indices,
            capacity,
        }
    }

    fn add_to_available_slot(&mut self, order_id: OrderId) -> usize {
        // if avail_index is not empty -> consume first
        let index = if !self.avail_indices.is_empty() {
            let index = self.avail_indices.pop_front().unwrap();
            self.orders[index] = Some(order_id);
            index
        } else {
            let index = self.orders.len();
            self.orders.push(Some(order_id));
            index
        };
        index
    }

    // return cursor to the orders
    pub fn add_order(&mut self, order: &Arc<Order>) -> usize {
        self.volume += order.remaining_quantity;
        self.order_count += 1;
        self.add_to_available_slot(order.order_id)
    }

    // Arc<Order> can be query from orders HashMap inside OrderBook struct
    pub fn remove_order(&mut self, cursor: usize, order: &Arc<Order>) {
        // reset slot to None
        self.orders[cursor] = None;
        // free the index
        self.avail_indices.push_back(cursor);
        // subtract the volume
        self.volume -= order.remaining_quantity;
        self.order_count -= 1;
    }

    pub fn get_shift_loc(&self) -> usize {
        // O(n) operation
        let mut shift: usize = 0;
        while *self.orders.get(shift).unwrap() == None {
            shift += 1;
        }
        shift
    }

    pub fn update_order(&mut self, orig_order: Arc<Order>, modified_order: Arc<Order>) {
        // only handle price level update
        // order update in order HashMap impl

        // 1. cannot be update if
        self.volume =
            self.volume - orig_order.remaining_quantity + modified_order.remaining_quantity

        // order count remain the same, order position remain the same
    }

    pub fn get_level_info(&self) -> LevelInfo {
        LevelInfo {
            price: self.price,
            volume: self.volume,
        }
    }
}

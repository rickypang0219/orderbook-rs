use std::boxed::Box;
use std::ptr::NonNull;
use std::sync::Arc;

use crate::orderbook::order::Order;
use crate::orderbook::types::{OrderId, Price, Quantity};

use intrusive_collections::linked_list::CursorMut;
use intrusive_collections::{intrusive_adapter, KeyAdapter, LinkedList, LinkedListLink};

#[derive(Debug)]
pub struct OrderNode {
    pub link: LinkedListLink,
    pub order: Arc<Order>,
}

#[derive(Debug)]
pub struct PriceLevel {
    pub price: Price,
    pub orders: LinkedList<OrderNodeAdapter>,
    pub volume: Quantity,
    pub order_count: usize,
}

#[derive(Debug)]
pub struct LevelInfo {
    pub price: Price,
    pub volume: Quantity,
}

pub struct OrderEntry {
    pub order: Arc<Order>,
    pub cursor: NonNull<OrderNode>, // pub cursor: CursorMut<'a, OrderNodeAdapter>,
}

impl OrderNode {
    pub fn new(order: Arc<Order>) -> Self {
        Self {
            link: LinkedListLink::new(),
            order,
        }
    }
}

// Register adapter
intrusive_adapter!(
    pub OrderNodeAdapter = Box<OrderNode>: OrderNode { link: LinkedListLink }
);

// Implement KeyAdapter
impl<'a> KeyAdapter<'a> for OrderNodeAdapter {
    type Key = OrderId;

    fn get_key(&self, value: &'a OrderNode) -> Self::Key {
        value.order.order_id
    }
}

impl PriceLevel {
    pub fn new(price: Price) -> Self {
        Self {
            price,
            orders: LinkedList::new(OrderNodeAdapter::new()),
            volume: 0,
            order_count: 0,
        }
    }

    /// Add an order to the back of the list
    pub fn add_order(&mut self, order: Arc<Order>) -> CursorMut<'_, OrderNodeAdapter> {
        let node = Box::new(OrderNode::new(order.clone()));
        self.volume += order.remaining_quantity;
        self.order_count += 1;
        self.orders.push_back(node);

        // Return a cursor pointing to the new back element
        let cursor = self.orders.cursor_mut();
        cursor
    }

    /// Remove an order at the cursor
    pub fn remove_order(
        &mut self,
        mut cursor: CursorMut<'_, OrderNodeAdapter>,
    ) -> Option<Arc<Order>> {
        if let Some(node) = cursor.remove() {
            self.volume -= node.order.remaining_quantity;
            self.order_count -= 1;
            Some(node.order)
        } else {
            None
        }
    }

    pub fn add_order_return_ptr(&mut self, order: Arc<Order>) -> NonNull<OrderNode> {
        self.volume += order.remaining_quantity;
        self.order_count += 1;

        // Push the Box<OrderNode> into the list (list owns it)
        self.orders.push_back(Box::new(OrderNode::new(order)));

        // Now get a pointer to the back element we just pushed
        // Safety: back().get() returns Some(&OrderNode) because we just pushed
        let ptr = self
            .orders
            .back()
            .get()
            .expect("just pushed, so back exists") as *const OrderNode
            as *mut OrderNode;
        // Create NonNull (safe because pointer is non-null)
        // cast to NonNull; no check - we know it's non-null
        unsafe { NonNull::new_unchecked(ptr) }
    }

    /// Remove by node pointer (returns Arc<Order> if removed)
    pub fn remove_by_ptr(&mut self, ptr: NonNull<OrderNode>) -> Option<Arc<Order>> {
        // Create cursor mut from ptr — this method consumes &mut self (the list).
        // Safety: ptr must point to a node that is currently in this list.
        let mut cursor = unsafe { self.orders.cursor_mut_from_ptr(ptr.as_ptr()) };
        if let Some(node) = cursor.remove() {
            self.volume -= node.order.remaining_quantity;
            self.order_count -= 1;
            Some(node.order)
        } else {
            None
        }
    }

    /// Get frontmost order
    pub fn front(&self) -> Option<&Arc<Order>> {
        self.orders.front().get().map(|node| &node.order)
    }

    /// Pop the first order
    pub fn pop_front(&mut self) -> Option<Arc<Order>> {
        if let Some(node) = self.orders.pop_front() {
            self.volume -= node.order.remaining_quantity;
            self.order_count -= 1;
            Some(node.order)
        } else {
            None
        }
    }

    pub fn update_order(
        &mut self,
        mut cursor: CursorMut<'_, OrderNodeAdapter>,
        new_quantity: Quantity,
    ) -> Option<Arc<Order>> {
        if let Some(old_node) = cursor.remove() {
            // Calculate delta
            let old_quantity = old_node.order.remaining_quantity;
            self.volume = self.volume - old_quantity + new_quantity;

            // Create updated order
            let mut new_order = (*old_node.order).clone();
            new_order.remaining_quantity = new_quantity;
            let new_arc = Arc::new(new_order);

            // Insert new node at the same place
            let new_node = Box::new(OrderNode::new(new_arc.clone()));
            cursor.insert_before(new_node);

            Some(new_arc)
        } else {
            None
        }
    }

    pub fn get_level_info(&self) -> LevelInfo {
        LevelInfo {
            price: self.price,
            volume: self.volume,
        }
    }
}

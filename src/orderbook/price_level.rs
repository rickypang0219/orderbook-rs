use std::rc::Rc;
use std::sync::Arc;

use crate::orderbook::order::{Order, Status};
use crate::orderbook::types::{OrderId, Price, Quantity};

use intrusive_collections::linked_list::CursorMut;
use intrusive_collections::{KeyAdapter, LinkedList, LinkedListLink, intrusive_adapter};

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
    /// Keeps the node alive while it is addressable through the order index.
    pub node: Rc<OrderNode>,
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
    pub OrderNodeAdapter = Rc<OrderNode>: OrderNode { link: LinkedListLink }
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
        let node = Rc::new(OrderNode::new(order.clone()));
        self.volume += order.remaining_quantity;
        self.order_count += 1;
        self.orders.push_back(node);

        self.orders.cursor_mut()
    }

    /// Remove an order at the cursor
    pub fn remove_order(
        &mut self,
        mut cursor: CursorMut<'_, OrderNodeAdapter>,
    ) -> Option<Arc<Order>> {
        if let Some(node) = cursor.remove() {
            self.volume -= node.order.remaining_quantity;
            self.order_count -= 1;
            Some(node.order.clone())
        } else {
            None
        }
    }

    pub fn add_order_return_handle(&mut self, order: Arc<Order>) -> Rc<OrderNode> {
        self.volume += order.remaining_quantity;
        self.order_count += 1;

        let node = Rc::new(OrderNode::new(order));
        self.orders.push_back(node.clone());
        node
    }

    /// Removes the node referenced by an indexed owning handle.
    pub fn remove_by_handle(&mut self, node: &Rc<OrderNode>) -> Option<Arc<Order>> {
        // SAFETY: OrderBook creates the indexed Rc at the same time it inserts a
        // clone into this list, updates both together on replacement, and calls
        // this method before the indexed handle is dropped. Therefore `node`
        // points to a live member of this exact list.
        let mut cursor = unsafe { self.orders.cursor_mut_from_ptr(Rc::as_ptr(node)) };
        if let Some(node) = cursor.remove() {
            self.volume -= node.order.remaining_quantity;
            self.order_count -= 1;
            Some(node.order.clone())
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
            Some(node.order.clone())
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
            let new_node = Rc::new(OrderNode::new(new_arc.clone()));
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

    pub fn update_front_order_quantity(&mut self, new_quantity: Quantity) -> Option<Quantity> {
        let mut cursor = self.orders.front_mut();

        if let Some(front_node) = cursor.get() {
            let old_quantity = front_node.order.remaining_quantity;
            let delta = old_quantity as i64 - new_quantity as i64;

            // Create updated order
            let mut updated_order = (*front_node.order).clone();
            updated_order.remaining_quantity = new_quantity;
            updated_order.executed_quantity += delta.max(0) as Quantity;
            updated_order.status = if new_quantity == 0 {
                Status::Filled
            } else {
                Status::PartiallyFilled
            };

            // Replace the node using cursor.replace()
            let updated_node = Rc::new(OrderNode::new(Arc::new(updated_order)));
            let _ = cursor.replace_with(updated_node);

            // Update price level volume
            self.volume = self.volume.saturating_sub(delta.unsigned_abs());

            Some(old_quantity)
        } else {
            None
        }
    }
}

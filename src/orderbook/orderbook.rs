use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::rc::Rc;

use crate::orderbook::order::Order;
use crate::orderbook::types::{OrderId, Price, Quantity};

struct LevelData {
    quantity: Quantity,
    count: Quantity,
}

struct OrderPointers(VecDeque<Rc<Order>>);

struct OrderEntry {
    order: Rc<Order>, // shared ownership
    location: usize,
}

pub struct OrderBook {
    data: HashMap<Price, LevelData>,
    bids: BTreeMap<Reverse<Price>, OrderPointers>,
    asks: BTreeMap<Price, OrderPointers>,
    orders: HashMap<OrderId, OrderEntry>,
}

impl OrderBook {
    pub fn init_book() -> Self {
        OrderBook {
            data: HashMap::new(),
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            orders: HashMap::new(),
        }
    }

    pub fn add_bids(mut self, price: Price, order: Rc<Order>) -> () {
        let key = Reverse(price);
        let pointers = self
            .bids
            .entry(key)
            .or_insert(OrderPointers(VecDeque::new()));
        let location = pointers.0.len();
        pointers.0.push_back(order.clone()); // Clone Rc for shared ownership

        let entry = OrderEntry {
            order: order.clone(),
            location,
        };
        self.orders.insert(*order.get_order_id(), entry); // Assume Order has public `id: OrderId`

        // Update level data (assume Order has public `quantity: Quantity`)
        let level = self.data.entry(price).or_insert(LevelData {
            quantity: 0,
            count: 0,
        });
        level.quantity += order.get_remaining_qty();
        level.count += 1;
    }
}

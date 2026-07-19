use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::sync::Arc;

use chrono::Utc;
use log::info;
use uuid::Uuid;

use crate::orderbook::order::{Order, OrderType, Side, Status};
use crate::orderbook::price_level::{OrderEntry, OrderNode, PriceLevel};
use crate::orderbook::types::{OrderId, Price, Quantity};

#[derive(Clone, Debug, PartialEq)]
pub struct Trade {
    pub trade_id: OrderId,
    pub bid_order_id: OrderId,
    pub ask_order_id: OrderId,
    pub price: Price,
    pub quantity: Quantity,
    pub timestamp: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum OrderBookError {
    #[error("Order not found: {order_id}")]
    OrderNotFound { order_id: OrderId },

    #[error("Invalid price: {price}")]
    InvalidPrice { price: Price },

    #[error("Invalid quantity: {quantity}")]
    InvalidQuantity { quantity: Quantity },

    #[error("Order already exists: {order_id}")]
    OrderAlreadyExists { order_id: OrderId },

    #[error("Price Level not found: {price}")]
    PriceLevelNotFound { price: Price },

    #[error("No PriceLevelRef not found: {price}")]
    PriceLevelRefNotFound { price: Price },
}

#[derive(Debug, Clone, Copy)]
struct PriceLevelRef {
    index: usize,
}

pub struct OrderBook {
    bids: BTreeMap<Reverse<Price>, PriceLevelRef>,
    asks: BTreeMap<Price, PriceLevelRef>,
    orders: HashMap<OrderId, OrderEntry>,
    by_price: HashMap<Price, PriceLevelRef>,
    price_levels: Vec<Option<PriceLevel>>,
    free_indices: VecDeque<usize>,
}

impl Default for OrderBook {
    fn default() -> Self {
        Self::new()
    }
}

impl Trade {
    pub fn new(
        bid_order_id: OrderId,
        ask_order_id: OrderId,
        price: Price,
        quantity: Quantity,
    ) -> Self {
        Trade {
            trade_id: Uuid::new_v4(),
            bid_order_id,
            ask_order_id,
            price,
            quantity,
            timestamp: Utc::now().timestamp_micros(),
        }
    }
}

impl OrderBook {
    pub fn new() -> Self {
        Self::with_capacity(0, 1024)
    }

    /// Creates a book with capacity reserved for a known workload.
    pub fn with_capacity(order_capacity: usize, price_level_capacity: usize) -> Self {
        let price_levels = Vec::with_capacity(price_level_capacity);
        let free_indices = VecDeque::with_capacity(price_level_capacity);

        OrderBook {
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            orders: HashMap::with_capacity(order_capacity),
            by_price: HashMap::with_capacity(price_level_capacity),
            price_levels,
            free_indices,
        }
    }

    fn add_order_to_book(&mut self, order: &Arc<Order>) {
        let (price_level_ref, is_new_level) = match self.by_price.get(&order.price) {
            None => {
                let index = if let Some(index) = self.free_indices.pop_front() {
                    self.price_levels[index] = Some(PriceLevel::new(order.price));
                    index
                } else {
                    let index = self.price_levels.len();
                    self.price_levels.push(Some(PriceLevel::new(order.price)));
                    index
                };

                // Create new level reference and append it to HashMap
                let level_ref = PriceLevelRef { index };
                self.by_price.insert(order.price, level_ref);
                (level_ref, true)
            }
            Some(price_level_ref) => (*price_level_ref, false),
        };

        // Find the PriceLevel using Index in PriceLevelRef
        let node = self.price_levels[price_level_ref.index]
            .as_mut()
            .expect("Price Level cannot be None!")
            .add_order_return_handle(order.clone());
        let order_entry = OrderEntry { node };
        self.orders.insert(order.order_id, order_entry);

        if is_new_level {
            match order.side {
                Side::Buy => self.bids.insert(Reverse(order.price), price_level_ref),
                Side::Sell => self.asks.insert(order.price, price_level_ref),
            };
        }
    }
    pub fn add_order(&mut self, order: &Arc<Order>) -> Result<Vec<Trade>, OrderBookError> {
        if self.orders.contains_key(&order.order_id) {
            return Err(OrderBookError::OrderAlreadyExists {
                order_id: order.order_id,
            });
        }
        if order.original_quantity == 0 {
            return Err(OrderBookError::InvalidQuantity {
                quantity: order.original_quantity,
            });
        }
        if order.order_type != OrderType::MarketOrder && order.price <= 0 {
            return Err(OrderBookError::InvalidPrice { price: order.price });
        }

        match order.order_type {
            OrderType::MarketOrder => self.match_market(order),
            OrderType::ImmediateOrCancel => self.match_order(order),
            OrderType::FillOrKill => self.match_fill_or_kill(order),
            _ => self.match_and_add_to_book(order),
        }
    }

    pub fn cancel_order(&mut self, order_id: OrderId) -> Result<(), OrderBookError> {
        let order_entry = self
            .orders
            .remove(&order_id)
            .ok_or(OrderBookError::OrderNotFound { order_id })?;

        let order = order_entry.node.order.clone();

        let level_ref = match order.side {
            Side::Buy => self.bids.get(&Reverse(order.price)).copied(),
            Side::Sell => self.asks.get(&order.price).copied(),
        }
        .ok_or(OrderBookError::PriceLevelRefNotFound { price: order.price })?;

        let level = self.price_levels[level_ref.index]
            .as_mut()
            .ok_or(OrderBookError::PriceLevelNotFound { price: order.price })?;
        level
            .remove_by_handle(&order_entry.node)
            .ok_or(OrderBookError::OrderNotFound { order_id })?;

        if level.order_count == 0 {
            match order.side {
                Side::Buy => {
                    self.bids.remove(&Reverse(order.price));
                }
                Side::Sell => {
                    self.asks.remove(&order.price);
                }
            }
            self.price_levels[level_ref.index] = None;
            self.free_indices.push_back(level_ref.index);
            self.by_price.remove(&order.price);
        }
        Ok(())
    }

    fn match_order(&mut self, order: &Arc<Order>) -> Result<Vec<Trade>, OrderBookError> {
        // Most incoming orders trade at only one or two levels. Reserving by the
        // total resting order count made every add O(n) in allocation volume and
        // was the main cause of the misleading ~150K adds/sec result.
        let mut trades = Vec::new();
        let order_price: Price = order.price;
        let mut remaining_quantity: Quantity = order.remaining_quantity;
        let order_type: OrderType = order.order_type;

        match order.side {
            Side::Buy => {
                while remaining_quantity > 0 {
                    let best_ask = if let Some((&price, _)) = self.asks.iter().next() {
                        price
                    } else {
                        // Price level does not exist -> break matching
                        break;
                    };

                    if order_price >= best_ask || order_type == OrderType::MarketOrder {
                        let trade = self.match_at_price_level_optimized(
                            best_ask,
                            order,
                            remaining_quantity,
                        )?;
                        remaining_quantity -= trade.quantity;
                        trades.push(trade);
                    } else {
                        break;
                    };
                    // sleep 0.5s for debug purpose
                    // thread::sleep(Duration::from_millis(500));
                }
            }
            Side::Sell => {
                while remaining_quantity > 0 {
                    let best_bid = if let Some((&Reverse(price), _)) = self.bids.iter().next() {
                        price
                    } else {
                        // Price level does not exist -> break matching
                        break;
                    };

                    if order_price <= best_bid || order_type == OrderType::MarketOrder {
                        let trade = self.match_at_price_level_optimized(
                            best_bid,
                            order,
                            remaining_quantity,
                        )?;
                        remaining_quantity -= trade.quantity;
                        trades.push(trade);
                    } else {
                        break;
                    };
                    // sleep 0.5s for debug purpose
                    // thread::sleep(Duration::from_millis(500));
                }
            }
        }
        Ok(trades)
    }

    fn match_at_price_level_optimized(
        &mut self,
        best_price: Price,
        incoming_order: &Arc<Order>,
        max_quantity: Quantity,
    ) -> Result<Trade, OrderBookError> {
        let level_ref = match incoming_order.side {
            Side::Buy => self.asks.get(&best_price).copied(),
            Side::Sell => self.bids.get(&Reverse(best_price)).copied(),
        }
        .ok_or(OrderBookError::PriceLevelNotFound { price: best_price })?;

        let level_index = level_ref.index;
        let (trade, resting_id, replacement, level_is_empty) = {
            let price_level = self.price_levels[level_index]
                .as_mut()
                .ok_or(OrderBookError::PriceLevelNotFound { price: best_price })?;

            // The matching path always consumes FIFO, so a safe front cursor is
            // sufficient; raw-pointer cursor construction is reserved for cancel.
            let mut cursor = price_level.orders.front_mut();
            let resting_order = cursor
                .get()
                .expect("cursor created from the front node must be valid")
                .order
                .clone();
            let trade_quantity = max_quantity.min(resting_order.remaining_quantity);
            let (bid_order_id, ask_order_id) = match incoming_order.side {
                Side::Buy => (incoming_order.order_id, resting_order.order_id),
                Side::Sell => (resting_order.order_id, incoming_order.order_id),
            };
            let trade = Trade::new(bid_order_id, ask_order_id, best_price, trade_quantity);

            let replacement = if trade_quantity == resting_order.remaining_quantity {
                cursor.remove();
                price_level.order_count -= 1;
                None
            } else {
                let mut updated_order = (*resting_order).clone();
                updated_order.remaining_quantity -= trade_quantity;
                updated_order.executed_quantity += trade_quantity;
                updated_order.status = Status::PartiallyFilled;
                updated_order.timestamp = Utc::now().timestamp_micros();
                let updated_order = Arc::new(updated_order);

                let updated_node = Rc::new(OrderNode::new(updated_order));
                let indexed_node = updated_node.clone();
                cursor
                    .replace_with(updated_node)
                    .expect("a cursor on the front node cannot be null");
                Some(indexed_node)
            };

            price_level.volume -= trade_quantity;
            (
                trade,
                resting_order.order_id,
                replacement,
                price_level.orders.is_empty(),
            )
        };

        if let Some(updated_node) = replacement {
            let entry = self
                .orders
                .get_mut(&resting_id)
                .ok_or(OrderBookError::OrderNotFound {
                    order_id: resting_id,
                })?;
            entry.node = updated_node;
        } else {
            self.orders.remove(&resting_id);
        }

        if level_is_empty {
            self.remove_empty_price_level(best_price, incoming_order)?;
        }

        Ok(trade)
    }

    fn remove_empty_price_level(
        &mut self,
        price: Price,
        order: &Arc<Order>,
    ) -> Result<(), OrderBookError> {
        match order.side {
            Side::Buy => {
                if let Some(price_level_ref) = self.asks.remove(&price) {
                    // reset to None
                    self.price_levels[price_level_ref.index] = None;
                    // store index for later reuse
                    self.free_indices.push_back(price_level_ref.index);
                    // remove by_price
                    self.by_price.remove(&price);
                } else {
                    return Err(OrderBookError::PriceLevelNotFound { price });
                }
            }
            Side::Sell => {
                if let Some(price_level_ref) = self.bids.remove(&Reverse(price)) {
                    self.price_levels[price_level_ref.index] = None;
                    self.free_indices.push_back(price_level_ref.index);
                    self.by_price.remove(&price);
                } else {
                    return Err(OrderBookError::PriceLevelNotFound { price });
                }
            }
        }
        Ok(())
    }

    fn match_and_add_to_book(&mut self, order: &Arc<Order>) -> Result<Vec<Trade>, OrderBookError> {
        let trades = self.match_order(order)?;

        let traded_quantity: Quantity = trades.iter().map(|trade| trade.quantity).sum();
        let remaining_quantity = order.remaining_quantity - traded_quantity;

        if remaining_quantity > 0 {
            let mut remaining_order = order.as_ref().clone();
            remaining_order.remaining_quantity = remaining_quantity;
            remaining_order.executed_quantity += traded_quantity;
            if traded_quantity > 0 {
                remaining_order.status = Status::PartiallyFilled;
                remaining_order.timestamp = Utc::now().timestamp_micros();
            }
            self.add_order_to_book(&Arc::new(remaining_order));
        }

        Ok(trades)
    }

    fn match_market(&mut self, order: &Arc<Order>) -> Result<Vec<Trade>, OrderBookError> {
        let aggressive_price = match order.side {
            Side::Buy => Price::MAX, // buy at any price
            Side::Sell => 0,         // sell at any price
        };

        let mut order_arc = order.as_ref().clone();
        order_arc.price = aggressive_price;
        self.match_order(&Arc::new(order_arc))
    }

    fn match_fill_or_kill(&mut self, order: &Arc<Order>) -> Result<Vec<Trade>, OrderBookError> {
        let available_quantity: Quantity = self.get_available_quantity(order);

        if available_quantity < order.remaining_quantity {
            info!("FOK order is canceled due to insufficient quantity!");
            Ok(Vec::new())
        } else {
            info!("Return FOK match orders");
            self.match_order(order)
        }
    }

    // Handy function to sum over volume over vector indices
    fn sum_volume_at<I>(&self, indices: I) -> Quantity
    where
        I: IntoIterator<Item = usize>,
    {
        indices
            .into_iter()
            .filter_map(|i| self.price_levels.get(i).and_then(|opt| opt.as_ref()))
            .map(|level| level.volume)
            .sum()
    }

    fn get_available_quantity(&self, order: &Arc<Order>) -> Quantity {
        match order.side {
            Side::Buy => self.sum_volume_at(
                self.asks
                    .range(..=order.price)
                    .map(|(_, level_ref)| level_ref.index),
            ),
            Side::Sell => self.sum_volume_at(
                self.bids
                    .range(..=Reverse(order.price))
                    .map(|(_, level_ref)| level_ref.index),
            ),
        }
    }

    /// Returns the number of orders currently resting in the book.
    pub fn resting_order_count(&self) -> usize {
        self.orders.len()
    }

    /// Returns the total remaining quantity across all resting orders.
    pub fn total_resting_quantity(&self) -> Quantity {
        self.price_levels
            .iter()
            .filter_map(Option::as_ref)
            .map(|level| level.volume)
            .sum()
    }

    /// Checks the structural and accounting invariants that make raw-pointer
    /// cancellation safe.
    pub fn validate_invariants(&self) -> Result<(), String> {
        if let (Some(best_bid), Some(best_ask)) = (self.get_best_bid(), self.get_best_ask())
            && best_bid >= best_ask
        {
            return Err(format!(
                "crossed book: best bid {best_bid} >= best ask {best_ask}"
            ));
        }

        let active_levels = self.price_levels.iter().flatten().count();
        if active_levels != self.bids.len() + self.asks.len() {
            return Err(format!(
                "active level count {active_levels} != bid levels {} + ask levels {}",
                self.bids.len(),
                self.asks.len()
            ));
        }
        if active_levels != self.by_price.len() {
            return Err(format!(
                "active level count {active_levels} != price index count {}",
                self.by_price.len()
            ));
        }

        let mut seen_orders = HashSet::with_capacity(self.orders.len());
        let mut node_count = 0usize;
        for (Reverse(price), level_ref) in &self.bids {
            node_count += self.validate_level(*price, Side::Buy, *level_ref, &mut seen_orders)?;
        }
        for (price, level_ref) in &self.asks {
            node_count += self.validate_level(*price, Side::Sell, *level_ref, &mut seen_orders)?;
        }

        if node_count != self.orders.len() {
            return Err(format!(
                "list node count {node_count} != order index count {}",
                self.orders.len()
            ));
        }

        let mut free_indices = HashSet::with_capacity(self.free_indices.len());
        for &index in &self.free_indices {
            if !free_indices.insert(index) {
                return Err(format!("duplicate free price-level index {index}"));
            }
            if self.price_levels.get(index).is_none_or(Option::is_some) {
                return Err(format!(
                    "free index {index} does not reference an empty slot"
                ));
            }
        }

        Ok(())
    }

    fn validate_level(
        &self,
        expected_price: Price,
        expected_side: Side,
        level_ref: PriceLevelRef,
        seen_orders: &mut HashSet<OrderId>,
    ) -> Result<usize, String> {
        let indexed_ref = self
            .by_price
            .get(&expected_price)
            .ok_or_else(|| format!("price {expected_price} missing from by_price index"))?;
        if indexed_ref.index != level_ref.index {
            return Err(format!(
                "price {expected_price} points to conflicting level indices"
            ));
        }

        let level = self
            .price_levels
            .get(level_ref.index)
            .and_then(Option::as_ref)
            .ok_or_else(|| format!("price {expected_price} points to an empty level"))?;
        if level.price != expected_price {
            return Err(format!(
                "level price {} != map price {expected_price}",
                level.price
            ));
        }

        let mut count = 0usize;
        let mut volume = 0u64;
        for node in level.orders.iter() {
            let order = &node.order;
            if order.price != expected_price || order.side != expected_side {
                return Err(format!(
                    "order {} is indexed under the wrong price or side",
                    order.order_id
                ));
            }
            if order.remaining_quantity == 0 {
                return Err(format!("filled order {} is still resting", order.order_id));
            }
            if order.remaining_quantity + order.executed_quantity != order.original_quantity {
                return Err(format!(
                    "order {} quantity accounting is inconsistent",
                    order.order_id
                ));
            }
            if !seen_orders.insert(order.order_id) {
                return Err(format!("duplicate resting order {}", order.order_id));
            }

            let entry = self
                .orders
                .get(&order.order_id)
                .ok_or_else(|| format!("order {} missing from order index", order.order_id))?;
            if !std::ptr::eq(entry.node.as_ref(), node) {
                return Err(format!("stale pointer for order {}", order.order_id));
            }
            if !Arc::ptr_eq(&entry.node.order, order) {
                return Err(format!("order {} has stale indexed data", order.order_id));
            }

            count += 1;
            volume = volume
                .checked_add(order.remaining_quantity)
                .ok_or_else(|| format!("volume overflow at price {expected_price}"))?;
        }

        if count != level.order_count {
            return Err(format!(
                "level {expected_price} node count {count} != stored count {}",
                level.order_count
            ));
        }
        if volume != level.volume {
            return Err(format!(
                "level {expected_price} summed volume {volume} != stored volume {}",
                level.volume
            ));
        }

        Ok(count)
    }

    pub fn get_best_bid(&self) -> Option<Price> {
        if let Some((Reverse(price), _)) = self.bids.iter().next() {
            info!("Best bid price: {}", price);
            Some(*price)
        } else {
            info!("No bid price available");

            None
        }
    }

    pub fn get_best_ask(&self) -> Option<Price> {
        if let Some((price, _)) = self.asks.iter().next() {
            info!("Best ask price: {}", price);
            Some(*price)
        } else {
            info!("No ask price available");
            None
        }
    }
}

#[cfg(test)]
mod orderbook_tests {
    use super::*;
    use proptest::prelude::*;

    fn order(
        id: u128,
        order_type: OrderType,
        side: Side,
        price: Price,
        quantity: Quantity,
    ) -> Arc<Order> {
        Arc::new(Order::with_id(
            Uuid::from_u128(id),
            order_type,
            side,
            price,
            quantity,
        ))
    }

    #[test]
    fn check_add_new_limit_order() {
        let mut test_ob = OrderBook::new();
        let limit_order = Arc::new(Order::new(OrderType::LimitOrder, Side::Buy, 10, 10));
        let trades = test_ob.add_order(&limit_order).unwrap();
        assert_eq!(trades, Vec::new());
    }

    #[test]
    fn check_add_new_limit_order_and_later_comsumed_by_market_order() {
        let mut test_ob = OrderBook::new();
        let limit_order = Arc::new(Order::new(OrderType::LimitOrder, Side::Buy, 10, 10));
        let market_order = Arc::new(Order::new(OrderType::MarketOrder, Side::Sell, 10, 10));

        // limit order first arrives to the OB
        {
            test_ob.add_order(&limit_order).unwrap();
        }
        // Market Order arrives later to consume the OB
        let trades = test_ob.add_order(&market_order).unwrap();
        let first_trade = trades.first().unwrap();
        assert_eq!(first_trade.price, 10);
        assert_eq!(first_trade.quantity, 10);
        assert_eq!(trades.len(), 1);
    }

    #[test]
    fn check_get_best_bid_ask_in_multiple_limit_orders() {
        let mut test_ob = OrderBook::new();
        {
            let buy_order_1 = Arc::new(Order::new(OrderType::LimitOrder, Side::Buy, 9, 10));
            let buy_order_2 = Arc::new(Order::new(OrderType::LimitOrder, Side::Buy, 8, 5));
            let buy_order_3 = Arc::new(Order::new(OrderType::LimitOrder, Side::Buy, 7, 3));

            test_ob.add_order(&buy_order_1).unwrap();
            test_ob.add_order(&buy_order_2).unwrap();
            test_ob.add_order(&buy_order_3).unwrap();
        }

        {
            let sell_order_1 = Arc::new(Order::new(OrderType::LimitOrder, Side::Sell, 10, 10));
            let sell_order_2 = Arc::new(Order::new(OrderType::LimitOrder, Side::Sell, 11, 5));
            let sell_order_3 = Arc::new(Order::new(OrderType::LimitOrder, Side::Sell, 12, 3));

            test_ob.add_order(&sell_order_1).unwrap();
            test_ob.add_order(&sell_order_2).unwrap();
            test_ob.add_order(&sell_order_3).unwrap();
        }
        assert_eq!(test_ob.get_best_bid().unwrap(), 9);
        assert_eq!(test_ob.get_best_ask().unwrap(), 10);
    }

    #[test]
    fn check_add_multiples_limit_order_and_later_comsumed_by_an_market_order() {
        let mut test_ob = OrderBook::new();
        let market_order = Arc::new(Order::new(OrderType::MarketOrder, Side::Sell, 0, 10));

        // limit order first arrives to the OB
        {
            let buy_order_1 = Arc::new(Order::new(OrderType::LimitOrder, Side::Buy, 9, 3));
            let buy_order_2 = Arc::new(Order::new(OrderType::LimitOrder, Side::Buy, 8, 5));
            let buy_order_3 = Arc::new(Order::new(OrderType::LimitOrder, Side::Buy, 7, 10));

            test_ob.add_order(&buy_order_1).unwrap();
            test_ob.add_order(&buy_order_2).unwrap();
            test_ob.add_order(&buy_order_3).unwrap();
        }
        // Market Order arrives later to consume the OB
        let trades = test_ob.add_order(&market_order).unwrap();
        assert_eq!(trades.len(), 3);
    }

    #[test]
    fn market_order_never_rests_unfilled_quantity() {
        let mut book = OrderBook::new();
        let market_order = order(1, OrderType::MarketOrder, Side::Buy, 0, 10);

        assert!(book.add_order(&market_order).unwrap().is_empty());
        assert_eq!(book.resting_order_count(), 0);
        assert!(book.validate_invariants().is_ok());
    }

    #[test]
    fn ioc_executes_available_quantity_and_never_rests_the_remainder() {
        let mut book = OrderBook::new();
        let ask = order(1, OrderType::GoodTillCancel, Side::Sell, 100, 5);
        let ioc = order(2, OrderType::ImmediateOrCancel, Side::Buy, 101, 10);
        book.add_order(&ask).unwrap();

        let trades = book.add_order(&ioc).unwrap();

        assert_eq!(trades.len(), 1);
        assert_eq!(trades[0].quantity, 5);
        assert_eq!(trades[0].bid_order_id, ioc.order_id);
        assert_eq!(trades[0].ask_order_id, ask.order_id);
        assert_eq!(book.resting_order_count(), 0);
        assert!(book.validate_invariants().is_ok());
    }

    #[test]
    fn fok_executes_when_exact_quantity_is_available() {
        let mut book = OrderBook::new();
        let ask = order(1, OrderType::GoodTillCancel, Side::Sell, 100, 10);
        let fok = order(2, OrderType::FillOrKill, Side::Buy, 100, 10);
        book.add_order(&ask).unwrap();

        let trades = book.add_order(&fok).unwrap();

        assert_eq!(trades.iter().map(|trade| trade.quantity).sum::<u64>(), 10);
        assert_eq!(book.resting_order_count(), 0);
        assert!(book.validate_invariants().is_ok());
    }

    #[test]
    fn fok_is_atomic_when_compatible_liquidity_is_insufficient() {
        let mut book = OrderBook::new();
        let ask_at_limit = order(1, OrderType::GoodTillCancel, Side::Sell, 100, 5);
        let ask_above_limit = order(2, OrderType::GoodTillCancel, Side::Sell, 101, 5);
        let fok = order(3, OrderType::FillOrKill, Side::Buy, 100, 6);
        book.add_order(&ask_at_limit).unwrap();
        book.add_order(&ask_above_limit).unwrap();

        assert!(book.add_order(&fok).unwrap().is_empty());
        assert_eq!(book.resting_order_count(), 2);
        assert_eq!(book.total_resting_quantity(), 10);
        assert!(book.validate_invariants().is_ok());
    }

    #[test]
    fn matching_preserves_fifo_within_a_price_level() {
        let mut book = OrderBook::new();
        let first_ask = order(1, OrderType::GoodTillCancel, Side::Sell, 100, 5);
        let second_ask = order(2, OrderType::GoodTillCancel, Side::Sell, 100, 5);
        let ioc = order(3, OrderType::ImmediateOrCancel, Side::Buy, 100, 6);
        book.add_order(&first_ask).unwrap();
        book.add_order(&second_ask).unwrap();

        let trades = book.add_order(&ioc).unwrap();

        assert_eq!(trades.len(), 2);
        assert_eq!(trades[0].ask_order_id, first_ask.order_id);
        assert_eq!(trades[0].quantity, 5);
        assert_eq!(trades[1].ask_order_id, second_ask.order_id);
        assert_eq!(trades[1].quantity, 1);
        assert_eq!(book.total_resting_quantity(), 4);
        assert!(book.validate_invariants().is_ok());
    }

    #[test]
    fn cancel_sell_removes_ask_level() {
        let mut book = OrderBook::new();
        let ask = order(1, OrderType::GoodTillCancel, Side::Sell, 100, 10);
        book.add_order(&ask).unwrap();

        book.cancel_order(ask.order_id).unwrap();

        assert_eq!(book.get_best_ask(), None);
        assert!(book.validate_invariants().is_ok());
    }

    #[test]
    fn partial_fill_updates_pointer_before_cancel() {
        let mut book = OrderBook::new();
        let ask = order(1, OrderType::GoodTillCancel, Side::Sell, 100, 10);
        let ioc = order(2, OrderType::ImmediateOrCancel, Side::Buy, 100, 4);
        book.add_order(&ask).unwrap();
        book.add_order(&ioc).unwrap();
        assert_eq!(book.total_resting_quantity(), 6);
        assert!(book.validate_invariants().is_ok());

        book.cancel_order(ask.order_id).unwrap();

        assert_eq!(book.resting_order_count(), 0);
        assert!(book.validate_invariants().is_ok());
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]

        #[test]
        fn random_operation_sequences_preserve_invariants(
            operations in prop::collection::vec(
                (0u8..6, any::<bool>(), 95i64..106, 1u64..25, 0usize..512),
                1..256,
            )
        ) {
            let mut book = OrderBook::new();
            let mut submitted_ids = Vec::new();

            for (step, (operation, buy, price, quantity, selector)) in
                operations.into_iter().enumerate()
            {
                if operation == 0 && !submitted_ids.is_empty() {
                    let id = submitted_ids[selector % submitted_ids.len()];
                    let _ = book.cancel_order(id);
                } else {
                    let side = if buy { Side::Buy } else { Side::Sell };
                    let order_type = match operation {
                        2 => OrderType::ImmediateOrCancel,
                        3 => OrderType::FillOrKill,
                        4 => OrderType::MarketOrder,
                        _ => OrderType::GoodTillCancel,
                    };
                    let order = order(
                        step as u128 + 1,
                        order_type,
                        side,
                        if order_type == OrderType::MarketOrder { 0 } else { price },
                        quantity,
                    );
                    submitted_ids.push(order.order_id);
                    prop_assert!(book.add_order(&order).is_ok());
                }

                prop_assert!(
                    book.validate_invariants().is_ok(),
                    "invariant failure after step {step}: {:?}",
                    book.validate_invariants()
                );
            }
        }
    }
}

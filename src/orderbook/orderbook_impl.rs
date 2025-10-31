use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ptr::NonNull;
use std::sync::Arc;
use std::{thread, time::Duration};

use chrono::Utc;
use intrusive_collections::LinkedListLink;
use log::{error, info};
use uuid::Uuid;

use crate::orderbook::order::{Order, OrderType, Side, Status};
use crate::orderbook::price_level::{OrderEntry, OrderNode, PriceLevel};
use crate::orderbook::types::{OrderId, Price, Quantity};

#[derive(Clone, Debug, PartialEq)]
pub struct Trade {
    trade_id: OrderId,
    bid_order_id: OrderId,
    ask_order_id: OrderId,
    price: Price,
    quantity: Quantity,
    timestamp: i64,
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
    price: Price,
}

pub struct OrderBook {
    bids: BTreeMap<Reverse<Price>, PriceLevelRef>,
    asks: BTreeMap<Price, PriceLevelRef>,
    orders: HashMap<OrderId, OrderEntry>,
    by_price: HashMap<Price, PriceLevelRef>,
    price_levels: Vec<Option<PriceLevel>>,
    free_indices: VecDeque<usize>,
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
        let init_capacity: usize = 1024;
        let price_levels: Vec<Option<PriceLevel>> = Vec::with_capacity(init_capacity);
        let free_indices: VecDeque<usize> = VecDeque::with_capacity(init_capacity);

        OrderBook {
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            orders: HashMap::new(),
            by_price: HashMap::new(),
            price_levels,
            free_indices,
        }
    }

    fn add_order_to_book(&mut self, order: &Arc<Order>) {
        let price_level_ref = match self.by_price.get(&order.price) {
            None => {
                let index: usize =
                    if (!self.free_indices.is_empty()) && (self.price_levels.len() == 1024) {
                        let index = self
                            .free_indices
                            .pop_front()
                            .expect("Free indices Vector Cannot be None!");
                        self.price_levels[index] = Some(PriceLevel::new(order.price));
                        index
                    } else {
                        let index = self.price_levels.len();
                        self.price_levels.push(Some(PriceLevel::new(order.price)));
                        index
                        // self.price_levels.len() - 1
                    };

                // Create new level reference and append it to HashMap
                let level_ref = PriceLevelRef {
                    index,
                    price: order.price,
                };
                self.by_price.insert(order.price, level_ref);
                level_ref
            }
            Some(price_level_ref) => *price_level_ref,
        };

        // Find the PriceLevel using Index in PriceLevelRef
        let cursor = self.price_levels[price_level_ref.index]
            .as_mut()
            .expect("Price Level cannot be None!")
            .add_order_return_ptr(order.clone());
        let order_entry = OrderEntry {
            order: order.clone(),
            cursor,
        };
        self.orders.insert(order.order_id, order_entry);

        // add the Level Reference by side
        match order.side {
            Side::Buy => self.bids.insert(Reverse(order.price), price_level_ref),
            Side::Sell => self.asks.insert(order.price, price_level_ref),
        };
    }
    // Should rename to handle order
    pub fn add_order(&mut self, order: &Arc<Order>) -> Result<Vec<Option<Trade>>, OrderBookError> {
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

        let mut trades: Vec<Option<Trade>> = Vec::with_capacity(self.orders.len());

        match order.order_type {
            OrderType::MarketOrder => trades = self.match_market(order).unwrap(),
            OrderType::ImmediateOrCancel => {}
            OrderType::FillOrKill => trades = self.match_fill_or_kill(order).unwrap(),
            _ => trades = self.match_and_add_to_book(order).unwrap(),
        }

        Ok(trades)
    }

    pub fn cancel_order(&mut self, order_id: OrderId) -> Result<(), OrderBookError> {
        let order_entry = self
            .orders
            .remove(&order_id)
            .ok_or(OrderBookError::OrderNotFound { order_id })?;

        let order = &order_entry.order;

        match order.side {
            Side::Buy => {
                let price_level_ref = { self.bids.get(&Reverse(order.price)) };
                let index: usize = price_level_ref.unwrap().index;
                let target_level = self.price_levels[index].as_mut().unwrap();
                target_level.remove_by_ptr(order_entry.cursor);
                if target_level.order_count == 0 {
                    self.price_levels[index] = None;
                    self.free_indices.push_back(index);
                    self.by_price.remove(&order.price);
                }
            }
            Side::Sell => {
                let price_level_ref = { self.bids.get(&Reverse(order.price)) };
                let index: usize = price_level_ref.unwrap().index;
                let target_level = self.price_levels[index].as_mut().unwrap();
                target_level.remove_by_ptr(order_entry.cursor);
                if target_level.order_count == 0 {
                    self.price_levels[index] = None;
                    self.free_indices.push_back(index);
                    self.by_price.remove(&order.price);
                }
            }
        }
        Ok(())
    }

    fn match_order(&mut self, order: &Arc<Order>) -> Result<Vec<Option<Trade>>, OrderBookError> {
        let mut trades: Vec<Option<Trade>> = Vec::with_capacity(self.orders.len());
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
                        let trade = self
                            .match_at_price_level_optimized(best_ask, order, remaining_quantity)
                            .unwrap();
                        remaining_quantity -= trade.quantity;
                        trades.push(Some(trade));
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
                        let trade = self
                            .match_at_price_level_optimized(best_bid, order, remaining_quantity)
                            .unwrap();
                        remaining_quantity -= trade.quantity;
                        trades.push(Some(trade));
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

    fn match_at_price_level(
        &mut self,
        best_price: Price,
        order: &Arc<Order>,
        max_quantity: Quantity,
    ) -> Result<Trade, OrderBookError> {
        let price_level_ref_opt = match order.side {
            Side::Buy => self.asks.get_mut(&best_price),
            Side::Sell => self.bids.get_mut(&Reverse(best_price)),
        };

        let price_level_ref = match price_level_ref_opt {
            Some(level) => level,
            None => {
                error!("Price Level Not Found at price {}", best_price);
                return Err(OrderBookError::PriceLevelNotFound { price: best_price });
            }
        };

        // 2️⃣ Get cursor to front node
        let index = price_level_ref.index;
        let target_level = self.price_levels[index].as_mut().unwrap();
        let front_cursor = target_level.orders.front();
        let node_ptr = match front_cursor.get() {
            Some(node) => node as *const OrderNode as *mut OrderNode,
            None => {
                return Err(OrderBookError::OrderNotFound {
                    order_id: order.order_id,
                });
            }
        };
        let node_ptr = unsafe { NonNull::new_unchecked(node_ptr) };

        // 3️⃣ Build mutable cursor from pointer
        let mut cursor = unsafe { target_level.orders.cursor_mut_from_ptr(node_ptr.as_ptr()) };

        // 4️⃣ Get old node
        let old_order_arc = cursor.get().expect("Node must exist").order.clone();

        // 5️⃣ Compute trade quantity and trade
        let trade_quantity = max_quantity.min(old_order_arc.remaining_quantity);
        let trade_price = old_order_arc.price;

        let trade = Trade::new(
            order.order_id,
            old_order_arc.order_id,
            trade_price,
            trade_quantity,
        );

        {
            target_level.volume -= trade_quantity;
        }

        let new_order_remaining_quantity = old_order_arc.remaining_quantity - trade_quantity;
        let new_order_executed_quantity = old_order_arc.executed_quantity + trade_quantity;

        // 7️⃣ Create new node with updated remaining quantity and other fields
        let new_order = Arc::new(Order {
            order_id: old_order_arc.order_id,
            order_type: old_order_arc.order_type,
            side: old_order_arc.side,
            status: if new_order_remaining_quantity == 0 {
                Status::Filled
            } else {
                Status::PartiallyFilled
            },
            price: old_order_arc.price,
            original_quantity: old_order_arc.original_quantity,
            remaining_quantity: new_order_remaining_quantity,
            executed_quantity: new_order_executed_quantity,
            timestamp: Utc::now().timestamp_micros(),
            // copy other fields if necessary
        });

        let new_node = OrderNode {
            link: LinkedListLink::new(),
            order: new_order.clone(),
        };

        // 8️⃣ Insert new node after old node
        cursor.insert_after(Box::new(new_node));

        // 9️⃣ Remove old node
        cursor.remove();

        // Remove filled order from price level
        if new_order.remaining_quantity == 0 {
            target_level.orders.pop_front(); // order removal
            target_level.order_count -= 1; // update order count
        }

        // Update HashMap entry
        if let Some(entry) = self.orders.get_mut(&old_order_arc.order_id) {
            entry.order = new_order;
        }

        //1️⃣ Remove empty price level if needed
        if target_level.orders.is_empty() {
            self.remove_empty_price_level(best_price, order);
        };
        Ok(trade)
    }

    fn match_at_price_level_optimized(
        &mut self,
        best_price: Price,
        incoming_order: &Arc<Order>,
        max_quantity: Quantity,
    ) -> Option<Trade> {
        let level_ref = match incoming_order.side {
            Side::Buy => self.asks.get(&best_price)?,
            Side::Sell => self.bids.get(&Reverse(best_price))?,
        };

        let level_index = level_ref.index;
        let price_level = self.price_levels[level_index].as_mut()?;

        // Get front order info
        let front_cursor = price_level.orders.front();
        let node_ptr = front_cursor
            .get()
            .map(|node| node as *const OrderNode as *mut OrderNode)?;
        let node_ptr = unsafe { NonNull::new_unchecked(node_ptr) };

        // Create cursor from pointer for mutation
        let mut cursor = unsafe { price_level.orders.cursor_mut_from_ptr(node_ptr.as_ptr()) };

        let resting_order = cursor.get()?.order.clone();
        let trade_quantity = max_quantity.min(resting_order.remaining_quantity);
        let trade_price = best_price;

        let trade = Trade::new(
            incoming_order.order_id,
            resting_order.order_id,
            trade_price,
            trade_quantity,
        );

        if trade_quantity == resting_order.remaining_quantity {
            // Full fill - remove order
            cursor.remove();
            price_level.volume -= trade_quantity;
            price_level.order_count -= 1;
            self.orders.remove(&resting_order.order_id);
        } else {
            // Partial fill - update using cursor.replace()
            let new_quantity = resting_order.remaining_quantity - trade_quantity;
            let mut updated_order = (*resting_order).clone();
            updated_order.remaining_quantity = new_quantity;
            updated_order.executed_quantity += trade_quantity;
            updated_order.status = Status::PartiallyFilled;

            let updated_node = Box::new(OrderNode::new(Arc::new(updated_order)));
            cursor.replace_with(updated_node);

            price_level.volume -= trade_quantity;
        }

        if price_level.orders.is_empty() {
            self.remove_empty_price_level(best_price, incoming_order);
        }

        Some(trade)
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

    fn match_and_add_to_book(
        &mut self,
        order: &Arc<Order>,
    ) -> Result<Vec<Option<Trade>>, OrderBookError> {
        let trades: Vec<Option<Trade>> = self.match_order(order).unwrap();

        let traded_quantity: Quantity = trades.iter().map(|t| t.as_ref().unwrap().quantity).sum();
        let remaining_quantity = order.remaining_quantity - traded_quantity;

        if remaining_quantity > 0 {
            let mut remaining_order = order.as_ref().clone();
            remaining_order.remaining_quantity = remaining_quantity;
            self.add_order_to_book(&Arc::new(remaining_order));
        }

        Ok(trades)
    }

    fn match_market(&mut self, order: &Arc<Order>) -> Result<Vec<Option<Trade>>, OrderBookError> {
        let aggressive_price = match order.side {
            Side::Buy => Price::MAX, // buy at any price
            Side::Sell => 0,         // sell at any price
        };

        let mut order_arc = order.as_ref().clone();
        order_arc.price = aggressive_price;
        self.match_order(&Arc::new(order_arc))
    }

    fn match_fill_or_kill(
        &mut self,
        order: &Arc<Order>,
    ) -> Result<Vec<Option<Trade>>, OrderBookError> {
        let available_quantity: Quantity = self.get_available_quantity(order);

        if available_quantity <= order.original_quantity {
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
        let side = order.side;
        let order_price = order.price;

        let indices: Vec<usize> = match side {
            Side::Buy => self
                .bids
                .range(..=Reverse(order_price))
                .map(|(_, level_ref)| level_ref.index)
                .collect(),
            Side::Sell => self
                .asks
                .range(..=order_price)
                .map(|(_, level_ref)| level_ref.index)
                .collect(),
        };
        self.sum_volume_at(indices)
    }

    pub fn get_best_bid(&self) -> Option<Price> {
        if let Some((Reverse(price), _)) = self.bids.iter().next() {
            info!("Best ask price: {}", price);
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
        assert_eq!(trades.iter().next().unwrap().price, 10);
        assert_eq!(trades.iter().next().unwrap().quantity, 10);
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
    fn check_consume_limit_order_by_market_order() {}

    #[test]
    fn check_consume_limit_order_by_ioc_order() {}

    #[test]
    fn check_consume_limit_order_by_fok_order() {}
}

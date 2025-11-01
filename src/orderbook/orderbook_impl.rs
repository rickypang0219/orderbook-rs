use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;
use std::{thread, time::Duration};

use chrono::Utc;
use log::{error, info};
use uuid::Uuid;

use crate::orderbook::order::{Order, OrderType, Side, Status};
use crate::orderbook::price_level::{OrderEntry, PriceLevel};
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
    pub price_levels_capacity: usize,
    pub orders_capacity: usize,
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
    pub fn new(price_levels_capacity: usize, orders_capacity: usize) -> Self {
        let price_levels: Vec<Option<PriceLevel>> = Vec::with_capacity(price_levels_capacity);
        let free_indices: VecDeque<usize> = VecDeque::with_capacity(price_levels_capacity);

        OrderBook {
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            orders: HashMap::new(),
            by_price: HashMap::new(),
            price_levels,
            free_indices,
            price_levels_capacity,
            orders_capacity,
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
                        self.price_levels[index] =
                            Some(PriceLevel::new(order.price, self.orders_capacity));
                        index
                    } else {
                        let index = self.price_levels.len();
                        self.price_levels
                            .push(Some(PriceLevel::new(order.price, self.orders_capacity)));
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

        let cursor: usize = self.price_levels[price_level_ref.index]
            .as_mut()
            .unwrap()
            .add_order(order);

        let order_entry = OrderEntry {
            order: order.clone(),
            order_cursor: cursor,
            price_level_cursor: price_level_ref.index,
        };

        self.orders.insert(order.order_id, order_entry);

        // add the Level Reference by side
        match order.side {
            Side::Buy => self.bids.insert(Reverse(order.price), price_level_ref),
            Side::Sell => self.asks.insert(order.price, price_level_ref),
        };
    }
    // Should rename to handle order
    pub fn handle_order(
        &mut self,
        order: &Arc<Order>,
    ) -> Result<Vec<Option<Trade>>, OrderBookError> {
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

    fn remove_order_from_price_level(&mut self, order: &Arc<Order>, order_entry: &OrderEntry) {
        match order.side {
            Side::Buy => {
                let price_level_ref = { self.bids.get(&Reverse(order.price)) };
                let index: usize = price_level_ref.unwrap().index;
                let target_level = self.price_levels[index].as_mut().unwrap();
                target_level.remove_order(order_entry.order_cursor, order);
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
                target_level.remove_order(order_entry.order_cursor, order);
                if target_level.order_count == 0 {
                    self.price_levels[index] = None;
                    self.free_indices.push_back(index);
                    self.by_price.remove(&order.price);
                }
            }
        }
    }

    pub fn cancel_order(&mut self, order_id: OrderId) -> Result<(), OrderBookError> {
        let order_entry = self
            .orders
            .remove(&order_id)
            .ok_or(OrderBookError::OrderNotFound { order_id })?;

        let order = &order_entry.order;
        self.remove_order_from_price_level(order, &order_entry);
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

        // Go to Vec<PriceLevel> using the index inside PriceLevelRef
        let level_index = level_ref.index;

        // unwrap the price level, but it could not be None
        // Since if price level is None, we should reject before
        // Add order simply add to book if GTC/Limit
        let price_level = self.price_levels[level_index].as_mut().unwrap();

        // println!("price level before pop {:?}", &price_level);

        // remove front item is none in order VecDeque
        let shift_loc = price_level.get_shift_loc();

        let order_id = price_level
            .orders
            .get(shift_loc)
            .expect("There must have orderIds if Pricelevel is not None/Newly created")
            .expect("OrderId cannot be None if PriceLevel is not None/Newly created");

        // println!("price level after pop {:?}", &price_level);

        let order_entry = self.orders.get(&order_id).expect("OrderId Must Exist!");

        let resting_order = &order_entry.order;
        let price_levels_cursor = order_entry.price_level_cursor;
        let orders_cursor = order_entry.order_cursor;

        // Matching rule
        let trade_quantity = max_quantity.min(resting_order.remaining_quantity);
        let trade_price = best_price;

        let trade = Trade::new(
            incoming_order.order_id,
            resting_order.order_id,
            trade_price,
            trade_quantity,
        );

        if trade_quantity == resting_order.remaining_quantity {
            price_level.remove_order(orders_cursor, &resting_order);
            // remove record from Orders HashMap
            self.orders.remove(&order_id);
        } else {
            let new_quantity = resting_order.remaining_quantity - trade_quantity;
            let mut updated_order = (**resting_order).clone();
            updated_order.remaining_quantity = new_quantity;
            updated_order.executed_quantity += trade_quantity;
            updated_order.status = Status::PartiallyFilled;

            // update orderEntry record
            let new_order_entry = OrderEntry {
                order: Arc::new(updated_order),
                price_level_cursor: price_levels_cursor,
                order_cursor: orders_cursor,
            };
            self.orders
                .entry(resting_order.order_id)
                .and_modify(|order_entry| *order_entry = new_order_entry);

            // only update the volume, order count remains the same
            price_level.volume -= trade_quantity;
            // if not filled consumed, add pack order_id
            // price_level.orders.push_front(Some(order_id));
        }

        if price_level.order_count == 0 {
            let _ = self.remove_empty_price_level(best_price, incoming_order);
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
        let mut test_ob = OrderBook::new(1024, 1024);
        let limit_order = Arc::new(Order::new(OrderType::LimitOrder, Side::Buy, 10, 10));
        let trades = test_ob.handle_order(&limit_order).unwrap();
        assert_eq!(trades, Vec::new());
    }

    #[test]
    fn check_add_new_limit_order_and_later_comsumed_by_market_order() {
        let mut test_ob = OrderBook::new(1024, 1024);
        let limit_order = Arc::new(Order::new(OrderType::LimitOrder, Side::Buy, 10, 10));
        let market_order = Arc::new(Order::new(OrderType::MarketOrder, Side::Sell, 10, 10));

        // limit order first arrives to the OB
        {
            test_ob.handle_order(&limit_order).unwrap();
        }
        // Market Order arrives later to consume the OB
        let trades = test_ob.handle_order(&market_order).unwrap();
        assert_eq!(trades.len(), 1);
    }

    #[test]
    fn check_get_best_bid_ask_in_multiple_limit_orders() {
        let mut test_ob = OrderBook::new(1024, 1024);
        {
            let buy_order_1 = Arc::new(Order::new(OrderType::LimitOrder, Side::Buy, 9, 10));
            let buy_order_2 = Arc::new(Order::new(OrderType::LimitOrder, Side::Buy, 8, 5));
            let buy_order_3 = Arc::new(Order::new(OrderType::LimitOrder, Side::Buy, 7, 3));

            test_ob.handle_order(&buy_order_1).unwrap();
            test_ob.handle_order(&buy_order_2).unwrap();
            test_ob.handle_order(&buy_order_3).unwrap();
        }

        {
            let sell_order_1 = Arc::new(Order::new(OrderType::LimitOrder, Side::Sell, 10, 10));
            let sell_order_2 = Arc::new(Order::new(OrderType::LimitOrder, Side::Sell, 11, 5));
            let sell_order_3 = Arc::new(Order::new(OrderType::LimitOrder, Side::Sell, 12, 3));

            test_ob.handle_order(&sell_order_1).unwrap();
            test_ob.handle_order(&sell_order_2).unwrap();
            test_ob.handle_order(&sell_order_3).unwrap();
        }
        assert_eq!(test_ob.get_best_bid().unwrap(), 9);
        assert_eq!(test_ob.get_best_ask().unwrap(), 10);
    }

    #[test]
    fn check_add_multiples_limit_order_and_later_comsumed_by_an_market_order() {
        let mut test_ob = OrderBook::new(1024, 1024);
        let market_order = Arc::new(Order::new(OrderType::MarketOrder, Side::Sell, 0, 10));

        // limit order first arrives to the OB
        {
            let buy_order_1 = Arc::new(Order::new(OrderType::LimitOrder, Side::Buy, 9, 3));
            let buy_order_2 = Arc::new(Order::new(OrderType::LimitOrder, Side::Buy, 8, 5));
            let buy_order_3 = Arc::new(Order::new(OrderType::LimitOrder, Side::Buy, 7, 10));

            test_ob.handle_order(&buy_order_1).unwrap();
            test_ob.handle_order(&buy_order_2).unwrap();
            test_ob.handle_order(&buy_order_3).unwrap();
        }
        // Market Order arrives later to consume the OB
        let trades = test_ob.handle_order(&market_order).unwrap();
        assert_eq!(trades.len(), 3);
    }

    #[test]
    fn check_consume_limit_order_by_market_order() {}

    #[test]
    fn check_consume_limit_order_by_ioc_order() {}

    #[test]
    fn check_consume_limit_order_by_fok_order() {}
}

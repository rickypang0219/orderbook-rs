use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap};
use std::ptr::NonNull;
use std::sync::Arc;
use std::{thread, time::Duration};

use chrono::Utc;
use intrusive_collections::LinkedListLink;
use log::{error, info};
use uuid::Uuid;

use crate::orderbook::order::{Order, Side, Status};
use crate::orderbook::price_level::{LevelInfo, OrderEntry, OrderNode, PriceLevel};
use crate::orderbook::types::{OrderId, Price, Quantity};

use super::order::OrderType;

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
}

pub struct OrderBook {
    data: HashMap<Price, LevelInfo>,
    bids: BTreeMap<Reverse<Price>, PriceLevel>,
    asks: BTreeMap<Price, PriceLevel>,
    orders: HashMap<OrderId, OrderEntry>,
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
        OrderBook {
            data: HashMap::new(),
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            orders: HashMap::new(),
        }
    }

    fn can_match(self, side: Side, price: Price) -> bool {
        match side {
            Side::Buy => {
                if self.asks.is_empty() {
                    return false;
                }
                let (best_ask, _) = self.asks.iter().next().expect("Asks not empty");
                price >= *best_ask
            }
            Side::Sell => {
                if self.bids.is_empty() {
                    return false;
                }
                let (Reverse(best_bid), _) = self.bids.iter().next().expect("Bids not empty");
                price <= *best_bid
            }
        }
    }

    pub fn add_order(&mut self, order: &Arc<Order>) -> Result<Vec<Trade>, OrderBookError> {
        if self.orders.contains_key(&order.order_id) {
            return Err(OrderBookError::OrderAlreadyExists {
                order_id: order.order_id,
            });
        }
        // order quantity cannot be 0
        if order.original_quantity == 0 {
            return Err(OrderBookError::InvalidQuantity {
                quantity: order.original_quantity,
            });
        }

        let mut trades: Vec<Trade> = Vec::with_capacity(self.orders.len());

        // info!("Try'na match incoming order {:?}", &order);
        match order.order_type {
            OrderType::MarketOrder => trades = self.match_market(order)?,
            OrderType::ImmediateOrCancel => {}
            OrderType::FillOrKill => trades = self.match_fill_or_kill(order)?,

            // Limit order just get filled or sit in the book
            _ => trades = self.match_and_add_to_book(order)?,
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
                if let Some(price_level) = self.bids.get_mut(&Reverse(order.price)) {
                    price_level.remove_by_ptr(order_entry.cursor);
                    if price_level.order_count == 0 {
                        self.bids.remove(&Reverse(order.price));
                    }
                }
            }
            Side::Sell => {
                if let Some(price_level) = self.asks.get_mut(&order.price) {
                    price_level.remove_by_ptr(order_entry.cursor);
                    if price_level.order_count == 0 {
                        self.asks.remove(&order.price);
                    }
                }
            }
        }
        Ok(())
    }

    fn add_to_book(&mut self, order: &Arc<Order>) -> Result<(), OrderBookError> {
        let cursor = match order.side {
            Side::Buy => {
                let level = self
                    .bids
                    .entry(Reverse(order.price))
                    .or_insert_with(|| PriceLevel::new(order.price));
                // level.volume += order.remaining_quantity;
                level.add_order_return_ptr(order.clone())
            }
            Side::Sell => {
                let level = self
                    .asks
                    .entry(order.price)
                    .or_insert_with(|| PriceLevel::new(order.price));
                // level.volume += order.remaining_quantity;
                level.add_order_return_ptr(order.clone())
            }
        };

        let order_entry = OrderEntry {
            order: order.clone(),
            cursor,
        };
        self.orders.insert(order.order_id, order_entry);
        Ok(())
    }

    fn match_order(&mut self, order: &Arc<Order>) -> Result<Vec<Trade>, OrderBookError> {
        let mut trades: Vec<Trade> = Vec::with_capacity(self.orders.len());
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
                        let trade =
                            self.match_at_price_level(best_ask, order, remaining_quantity)?;
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
                        let trade =
                            self.match_at_price_level(best_bid, order, remaining_quantity)?;
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

    // fn match_at_price_level(
    //     &mut self,
    //     best_price: Price,
    //     order: &Arc<Order>,
    //     max_quantity: Quantity,
    // ) -> Result<Trade, OrderBookError> {
    //     let price_level = match order.side {
    //         Side::Buy => self.asks.get_mut(&best_price),
    //         Side::Sell => self.bids.get_mut(&Reverse(best_price)),
    //     };
    //
    //     match price_level {
    //         Some(price_level) => {
    //             let resting_order_node = price_level.orders.front().get();
    //             let resting_order_opt = resting_order_node.map(|node| &node.order);
    //             // let resting_order_opt = price_level.orders.front().get().map(|node| &node.order);
    //             if let Some(resting_order) = resting_order_opt {
    //                 let trade;
    //                 let is_fully_consumed;
    //                 {
    //                     let consumed_order = resting_order;
    //                     let trade_quantity = max_quantity.min(consumed_order.remaining_quantity);
    //                     let trade_price = consumed_order.price.clone();
    //
    //                     trade = Trade::new(
    //                         order.order_id,
    //                         consumed_order.order_id,
    //                         trade_price,
    //                         trade_quantity,
    //                     );
    //
    //                     // Update price level volume
    //                     info!(
    //                         "Trade quantitiy {} volume: {}",
    //                         trade_quantity, &price_level.volume
    //                     );
    //                     price_level.volume -= trade_quantity;
    //
    //                     // Update consumed order quantities
    //                     // use Arc shallow clone instead of Mutex Locking
    //                     let mut updated_order = consumed_order.as_ref().clone();
    //                     updated_order.remaining_quantity -= trade_quantity; // Mutation via clone
    //                     is_fully_consumed = updated_order.remaining_quantity == 0;
    //                     {
    //                         // replace update order in the orders Linked List
    //                         price_level.orders.pop_front();
    //                     }
    //                 }
    //                 if is_fully_consumed {
    //                     price_level.orders.pop_front();
    //                 }
    //
    //                 if price_level.orders.is_empty() {
    //                     if order.side == Side::Buy {
    //                         self.asks.remove(&best_price);
    //                     } else {
    //                         self.bids.remove(&Reverse(best_price));
    //                     }
    //                 }
    //
    //                 Ok(trade)
    //             } else {
    //                 error!("Order Not Found {}", &order.order_id);
    //                 Err(OrderBookError::OrderNotFound {
    //                     order_id: order.order_id,
    //                 })
    //             }
    //         }
    //         None => {
    //             error!("Price Level Not Found at price {}", best_price);
    //             Err(OrderBookError::PriceLevelNotFound { price: best_price })
    //         }
    //     }
    // }

    fn match_at_price_level(
        &mut self,
        best_price: Price,
        order: &Arc<Order>,
        max_quantity: Quantity,
    ) -> Result<Trade, OrderBookError> {
        // 1️⃣ Get mutable price level
        let price_level_opt = match order.side {
            Side::Buy => self.asks.get_mut(&best_price),
            Side::Sell => self.bids.get_mut(&Reverse(best_price)),
        };

        let price_level = match price_level_opt {
            Some(level) => level,
            None => {
                error!("Price Level Not Found at price {}", best_price);
                return Err(OrderBookError::PriceLevelNotFound { price: best_price });
            }
        };

        // 2️⃣ Get cursor to front node
        let front_cursor = price_level.orders.front();
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
        let mut cursor = unsafe { price_level.orders.cursor_mut_from_ptr(node_ptr.as_ptr()) };

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

        // info!("Trade Info {:?}", &trade);

        // 6️⃣For debug purpose
        // info!(
        //     "Trade quantity {} volume: {}",
        //     trade_quantity, price_level.volume
        // );

        price_level.volume -= trade_quantity;

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
            price_level.orders.pop_front();
        }

        // 🔟 Update HashMap entry
        if let Some(entry) = self.orders.get_mut(&old_order_arc.order_id) {
            entry.order = new_order;
        }

        // 1️⃣1️⃣ Remove empty price level if needed
        if price_level.orders.is_empty() {
            match order.side {
                Side::Buy => {
                    self.asks.remove(&best_price);
                }
                Side::Sell => {
                    self.bids.remove(&Reverse(best_price));
                }
            }
        };

        Ok(trade)
    }

    fn match_and_add_to_book(&mut self, order: &Arc<Order>) -> Result<Vec<Trade>, OrderBookError> {
        let trades: Vec<Trade> = self.match_order(order).unwrap();

        let traded_quantity: Quantity = trades.iter().map(|t| t.quantity).sum();
        let remaining_quantity = order.remaining_quantity - traded_quantity;

        if remaining_quantity > 0 {
            // create a new order with the remaining quantity
            let mut remaining_order = order.as_ref().clone();
            remaining_order.remaining_quantity = remaining_quantity;
            self.add_to_book(&Arc::new(remaining_order))?;
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

        if available_quantity <= order.original_quantity {
            info!("FOK order is canceled due to insufficient quantity!");
            Ok(Vec::new())
        } else {
            info!("Return FOK match orders");
            self.match_order(order)
        }
    }

    fn get_available_quantity(&self, order: &Arc<Order>) -> Quantity {
        let side = order.side;
        let order_price = order.price;

        match side {
            Side::Buy => self
                .asks
                .iter()
                .filter(|&(price, _)| *price <= order_price)
                .map(|(_, level)| level.volume)
                .sum(),
            Side::Sell => self
                .bids
                .iter()
                .filter(|(Reverse(price), _)| *price >= order_price)
                .map(|(_, level)| level.volume)
                .sum(),
        }
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

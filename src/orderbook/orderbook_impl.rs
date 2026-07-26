use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ptr::NonNull;
use std::sync::Arc;

use chrono::Utc;
use uuid::Uuid;

use crate::orderbook::order::{Order, OrderType, Side, Status};
use crate::orderbook::price_level::{OrderEntry, OrderNode, PriceLevel};
use crate::orderbook::types::{OrderId, Price, Quantity};

/// 初始容量。呢個係業務參數，唔係實作細節：
/// 「呢個 book 最多同時 hold 幾多個價位」應該明文寫低、monitor、alert。
const INIT_LEVEL_CAPACITY: usize = 1024;
/// 單一次 `add_order` 最多可以產生幾多筆成交。
const DEFAULT_TRADE_CAPACITY: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Trade {
    pub trade_id: OrderId,
    pub bid_order_id: OrderId,
    pub ask_order_id: OrderId,
    pub price: Price,
    pub quantity: Quantity,
    pub timestamp: i64,
}

impl Trade {
    /// 用嚟預先填滿 `TradeBuf` 嘅 backing store。
    pub const EMPTY: Trade = Trade {
        trade_id: Uuid::nil(),
        bid_order_id: Uuid::nil(),
        ask_order_id: Uuid::nil(),
        price: 0,
        quantity: 0,
        timestamp: 0,
    };

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

/// 固定容量嘅成交輸出 buffer。
///
/// 由 caller 持有並重用：每次前 `clear()`，之後讀 `as_slice()`。
/// 一旦 `with_capacity` 之後就永遠唔會再 allocate —— 滿咗係 backpressure
/// (`TradeBufferFull`)，唔係悄悄 grow。
#[derive(Debug)]
pub struct TradeBuf {
    buf: Box<[Trade]>,
    len: usize,
}

impl TradeBuf {
    pub fn with_capacity(cap: usize) -> Self {
        TradeBuf {
            buf: vec![Trade::EMPTY; cap].into_boxed_slice(),
            len: 0,
        }
    }

    #[inline(always)]
    pub fn clear(&mut self) {
        self.len = 0;
    }

    #[inline(always)]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline(always)]
    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    #[inline(always)]
    pub fn as_slice(&self) -> &[Trade] {
        &self.buf[..self.len]
    }

    /// 返回 false 代表 buffer 滿。呢度**唔會** grow。
    #[inline(always)]
    pub fn push(&mut self, t: Trade) -> bool {
        if self.len == self.buf.len() {
            return false;
        }
        self.buf[self.len] = t;
        self.len += 1;
        true
    }
}

impl Default for TradeBuf {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_TRADE_CAPACITY)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum OrderBookError {
    #[error("Order not found: {order_id}")]
    OrderNotFound { order_id: OrderId },

    #[error("Invalid price: {price}")]
    InvalidPrice { price: Price },

    #[error("Invalid quantity: {quantity}")]
    InvalidQuantity { quantity: Quantity },

    #[error("Order already exists: {order_id}")]
    OrderAlreadyExists { order_id: OrderId },

    #[error("Price level not found: {price}")]
    PriceLevelNotFound { price: Price },

    #[error("Trade buffer full")]
    TradeBufferFull,
}

#[derive(Debug, Clone, Copy)]
struct PriceLevelRef {
    index: usize,
}

pub struct OrderBook {
    bids: BTreeMap<Reverse<Price>, PriceLevelRef>,
    asks: BTreeMap<Price, PriceLevelRef>,
    orders: HashMap<OrderId, OrderEntry>,
    price_levels: Vec<Option<PriceLevel>>,
    free_indices: VecDeque<usize>,
}

impl Default for OrderBook {
    fn default() -> Self {
        Self::new()
    }
}

impl OrderBook {
    pub fn new() -> Self {
        OrderBook {
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            orders: HashMap::with_capacity(INIT_LEVEL_CAPACITY),
            price_levels: Vec::with_capacity(INIT_LEVEL_CAPACITY),
            free_indices: VecDeque::with_capacity(INIT_LEVEL_CAPACITY),
        }
    }

    // ---------------------------------------------------------------- helpers

    /// 攞某一 side 上某個價位嘅 level index。
    /// `by_price` HashMap 已刪除 —— bids/asks 本身就係 price -> ref 嘅 map，
    /// 用返佢哋就唔會再有「兩邊共用同一個 level」嘅 aliasing bug。
    #[inline]
    fn level_index(&self, side: Side, price: Price) -> Option<usize> {
        match side {
            Side::Buy => self.bids.get(&Reverse(price)).map(|r| r.index),
            Side::Sell => self.asks.get(&price).map(|r| r.index),
        }
    }

    /// 攞或者開一個 slot。free list 而家真係會被重用
    /// （原本個 `price_levels.len() == 1024` 條件永遠唔會成立）。
    fn acquire_level(&mut self, price: Price) -> usize {
        if let Some(index) = self.free_indices.pop_front() {
            self.price_levels[index] = Some(PriceLevel::new(price));
            index
        } else {
            let index = self.price_levels.len();
            self.price_levels.push(Some(PriceLevel::new(price)));
            index
        }
    }

    /// `level_side` 係**個 level 本身住喺邊一邊**，唔係 taker 嘅 side。
    /// 原本嘅 `remove_empty_price_level` 把兩者混埋一齊，係 cancel bug 嘅根源。
    fn release_level(&mut self, level_side: Side, price: Price) {
        let removed = match level_side {
            Side::Buy => self.bids.remove(&Reverse(price)),
            Side::Sell => self.asks.remove(&price),
        };
        if let Some(r) = removed {
            self.price_levels[r.index] = None;
            self.free_indices.push_back(r.index);
        }
    }

    // ------------------------------------------------------------ book mutate

    fn add_order_to_book(&mut self, order: &Arc<Order>) {
        let index = match self.level_index(order.side, order.price) {
            Some(i) => i,
            None => {
                let i = self.acquire_level(order.price);
                let r = PriceLevelRef { index: i };
                match order.side {
                    Side::Buy => self.bids.insert(Reverse(order.price), r),
                    Side::Sell => self.asks.insert(order.price, r),
                };
                i
            }
        };

        let cursor = self.price_levels[index]
            .as_mut()
            .expect("level just acquired")
            .add_order_return_ptr(order.clone());

        self.orders.insert(
            order.order_id,
            OrderEntry {
                order: order.clone(),
                cursor,
            },
        );
    }

    /// 成交結果寫入 `out`，唔再 return `Vec`。
    ///
    /// Caller 持有並重用同一個 `TradeBuf`：
    /// ```ignore
    /// let mut buf = TradeBuf::default();
    /// loop {
    ///     buf.clear();
    ///     book.add_order(&order, &mut buf)?;
    ///     sink.send(buf.as_slice());
    /// }
    /// ```
    pub fn add_order(
        &mut self,
        order: &Arc<Order>,
        out: &mut TradeBuf,
    ) -> Result<(), OrderBookError> {
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

        // Step 1: 原本呢度有一個 `Vec::with_capacity(self.orders.len())`，
        // 之後即刻被 match arm 整個覆蓋 —— 100% 浪費嘅 O(n) allocation。
        match order.order_type {
            OrderType::MarketOrder => {
                self.match_market(order, out)?;
            }
            OrderType::ImmediateOrCancel => {
                // IOC：食得幾多得幾多，餘數唔掛單
                self.match_order(
                    order.side,
                    order.price,
                    order.order_id,
                    order.remaining_quantity,
                    false,
                    out,
                )?;
            }
            OrderType::FillOrKill => {
                self.match_fill_or_kill(order, out)?;
            }
            _ => {
                self.match_and_add_to_book(order, out)?;
            }
        }

        Ok(())
    }

    pub fn cancel_order(&mut self, order_id: OrderId) -> Result<(), OrderBookError> {
        let entry = self
            .orders
            .remove(&order_id)
            .ok_or(OrderBookError::OrderNotFound { order_id })?;

        let side = entry.order.side;
        let price = entry.order.price;

        // 原本 Sell 分支查 `self.bids` —— side 寫錯，加 `.unwrap()` 會 panic。
        let index = self
            .level_index(side, price)
            .ok_or(OrderBookError::PriceLevelNotFound { price })?;

        let now_empty = {
            let level = self.price_levels[index]
                .as_mut()
                .ok_or(OrderBookError::PriceLevelNotFound { price })?;
            level.remove_by_ptr(entry.cursor);
            level.order_count == 0
        };

        if now_empty {
            self.release_level(side, price);
        }
        Ok(())
    }

    // ---------------------------------------------------------------- matching

    /// 用純量參數，唔再收 `&Arc<Order>`。
    /// 咁 `match_market` 就唔使為咗改一個 price field 而 clone + `Arc::new` 一次。
    /// 返回實際成交總量。
    fn match_order(
        &mut self,
        taker_side: Side,
        limit_price: Price,
        taker_id: OrderId,
        quantity: Quantity,
        is_market: bool,
        out: &mut TradeBuf,
    ) -> Result<Quantity, OrderBookError> {
        let mut remaining = quantity;
        let mut traded: Quantity = 0;

        while remaining > 0 {
            let best = match taker_side {
                Side::Buy => match self.asks.keys().next() {
                    Some(&p) => p,
                    None => break,
                },
                Side::Sell => match self.bids.keys().next() {
                    Some(&Reverse(p)) => p,
                    None => break,
                },
            };

            let crosses = is_market
                || match taker_side {
                    Side::Buy => limit_price >= best,
                    Side::Sell => limit_price <= best,
                };
            if !crosses {
                break;
            }

            let trade = match self.match_at_price_level(best, taker_side, taker_id, remaining) {
                Some(t) => t,
                None => break,
            };

            remaining -= trade.quantity;
            traded += trade.quantity;

            if !out.push(trade) {
                return Err(OrderBookError::TradeBufferFull);
            }
        }

        Ok(traded)
    }

    fn match_at_price_level(
        &mut self,
        best_price: Price,
        taker_side: Side,
        taker_id: OrderId,
        max_quantity: Quantity,
    ) -> Option<Trade> {
        // taker 買 -> 食 asks；taker 賣 -> 食 bids
        let maker_side = match taker_side {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        };
        let index = self.level_index(maker_side, best_price)?;

        // 呢個 block 借用 self.price_levels；出咗 block 先掂 self.orders。
        let (trade, resting_id, filled, updated) = {
            let level = self.price_levels[index].as_mut()?;

            let node_ptr = level
                .orders
                .front()
                .get()
                .map(|n| n as *const OrderNode as *mut OrderNode)?;
            let mut cursor = unsafe { level.orders.cursor_mut_from_ptr(node_ptr) };

            let resting = cursor.get()?.order.clone();
            let qty = max_quantity.min(resting.remaining_quantity);

            // 原本無論 taker 係買定賣都把 taker 當成 bid，trade 嘅雙邊 ID 會錯。
            let (bid_id, ask_id) = match taker_side {
                Side::Buy => (taker_id, resting.order_id),
                Side::Sell => (resting.order_id, taker_id),
            };
            let trade = Trade::new(bid_id, ask_id, best_price, qty);

            if qty == resting.remaining_quantity {
                cursor.remove();
                level.volume -= qty;
                level.order_count -= 1;
                (trade, resting.order_id, true, None)
            } else {
                let mut updated_order = (*resting).clone();
                updated_order.remaining_quantity -= qty;
                updated_order.executed_quantity += qty;
                updated_order.status = Status::PartiallyFilled;

                let node = Box::new(OrderNode::new(Arc::new(updated_order)));
                let _ = cursor.replace_with(node);
                level.volume -= qty;

                // replace_with 之後舊個 Box 已經 free，
                // self.orders 入面嗰個 NonNull 會變 dangling -> 要即刻更新。
                // (step 5 個 arena 會令呢個問題根本唔存在)
                let n = cursor.get().expect("just replaced");
                let new_ptr =
                    unsafe { NonNull::new_unchecked(n as *const OrderNode as *mut OrderNode) };
                let new_arc = n.order.clone();
                (trade, resting.order_id, false, Some((new_arc, new_ptr)))
            }
        };

        if filled {
            self.orders.remove(&resting_id);
        } else if let Some((arc, ptr)) = updated {
            if let Some(e) = self.orders.get_mut(&resting_id) {
                e.order = arc;
                e.cursor = ptr;
            }
        }

        let empty = self.price_levels[index]
            .as_ref()
            .map(|l| l.order_count == 0)
            .unwrap_or(true);
        if empty {
            self.release_level(maker_side, best_price);
        }

        Some(trade)
    }

    fn match_and_add_to_book(
        &mut self,
        order: &Arc<Order>,
        out: &mut TradeBuf,
    ) -> Result<(), OrderBookError> {
        let traded = self.match_order(
            order.side,
            order.price,
            order.order_id,
            order.remaining_quantity,
            false,
            out,
        )?;

        let remaining = order.remaining_quantity - traded;
        if remaining > 0 {
            let mut rest = order.as_ref().clone();
            rest.remaining_quantity = remaining;
            // 原本冇更新 executed_quantity / status，掛落 book 嘅 state 唔一致
            rest.executed_quantity = order.original_quantity - remaining;
            rest.status = if traded > 0 {
                Status::PartiallyFilled
            } else {
                Status::New
            };
            self.add_order_to_book(&Arc::new(rest));
        }
        Ok(())
    }

    fn match_market(
        &mut self,
        order: &Arc<Order>,
        out: &mut TradeBuf,
    ) -> Result<(), OrderBookError> {
        // 原本 Sell 用 0 做 aggressive price，但 Price = i64 係有符號嘅
        // （負價係真嘢）。而家用純量參數，連 clone + Arc::new 都慳返。
        let aggressive = match order.side {
            Side::Buy => Price::MAX,
            Side::Sell => Price::MIN,
        };
        self.match_order(
            order.side,
            aggressive,
            order.order_id,
            order.remaining_quantity,
            true,
            out,
        )?;
        Ok(())
    }

    fn match_fill_or_kill(
        &mut self,
        order: &Arc<Order>,
        out: &mut TradeBuf,
    ) -> Result<(), OrderBookError> {
        // 原本係 `available <= original_quantity` -> 啱啱夠成交嘅 FOK 都會被 kill
        if self.available_quantity(order.side, order.price) < order.remaining_quantity {
            return Ok(());
        }
        self.match_order(
            order.side,
            order.price,
            order.order_id,
            order.remaining_quantity,
            false,
            out,
        )?;
        Ok(())
    }

    /// Step 4：唔再 `.collect::<Vec<usize>>()`，而且修返兩邊反轉嘅 bug。
    ///
    /// - Buy taker 食 **asks**，價位 <= 自己嘅 limit
    /// - Sell taker 食 **bids**，價位 >= 自己嘅 limit
    ///   （bids 用 `Reverse` 做 key，所以 `..=Reverse(p)` 正正係 price >= p）
    fn available_quantity(&self, taker_side: Side, limit: Price) -> Quantity {
        let levels = &self.price_levels;
        match taker_side {
            Side::Buy => self
                .asks
                .range(..=limit)
                .filter_map(|(_, r)| levels.get(r.index).and_then(|o| o.as_ref()))
                .map(|l| l.volume)
                .sum(),
            Side::Sell => self
                .bids
                .range(..=Reverse(limit))
                .filter_map(|(_, r)| levels.get(r.index).and_then(|o| o.as_ref()))
                .map(|l| l.volume)
                .sum(),
        }
    }

    // ------------------------------------------------------------------ query

    // 原本呢度有 `info!(...)`：一旦 log level 開到 Info，每次查 best price
    // 都會 format 一個 String。hot path 唔應該有任何 formatting。
    #[inline]
    pub fn get_best_bid(&self) -> Option<Price> {
        self.bids.keys().next().map(|&Reverse(p)| p)
    }

    #[inline]
    pub fn get_best_ask(&self) -> Option<Price> {
        self.asks.keys().next().copied()
    }

    #[inline]
    pub fn order_count(&self) -> usize {
        self.orders.len()
    }

    #[inline]
    pub fn level_count(&self) -> usize {
        self.bids.len() + self.asks.len()
    }

    #[cfg(test)]
    fn slot_count(&self) -> usize {
        self.price_levels.len()
    }
}

#[cfg(test)]
mod orderbook_tests {
    use super::*;

    fn buf() -> TradeBuf {
        TradeBuf::with_capacity(64)
    }

    #[test]
    fn check_add_new_limit_order() {
        let mut ob = OrderBook::new();
        let mut b = buf();
        let o = Arc::new(Order::new(OrderType::LimitOrder, Side::Buy, 10, 10));
        ob.add_order(&o, &mut b).unwrap();
        assert!(b.is_empty());
        assert_eq!(ob.get_best_bid(), Some(10));
    }

    #[test]
    fn check_add_new_limit_order_and_later_comsumed_by_market_order() {
        let mut ob = OrderBook::new();
        let mut b = buf();

        let limit = Arc::new(Order::new(OrderType::LimitOrder, Side::Buy, 10, 10));
        ob.add_order(&limit, &mut b).unwrap();

        b.clear();
        let market = Arc::new(Order::new(OrderType::MarketOrder, Side::Sell, 0, 10));
        ob.add_order(&market, &mut b).unwrap();

        assert_eq!(b.len(), 1);
        assert_eq!(b.as_slice()[0].price, 10);
        assert_eq!(b.as_slice()[0].quantity, 10);
        assert_eq!(ob.get_best_bid(), None);
    }

    #[test]
    fn check_get_best_bid_ask_in_multiple_limit_orders() {
        let mut ob = OrderBook::new();
        let mut b = buf();
        for (side, price, qty) in [
            (Side::Buy, 9, 10),
            (Side::Buy, 8, 5),
            (Side::Buy, 7, 3),
            (Side::Sell, 10, 10),
            (Side::Sell, 11, 5),
            (Side::Sell, 12, 3),
        ] {
            b.clear();
            let o = Arc::new(Order::new(OrderType::LimitOrder, side, price, qty));
            ob.add_order(&o, &mut b).unwrap();
        }
        assert_eq!(ob.get_best_bid(), Some(9));
        assert_eq!(ob.get_best_ask(), Some(10));
    }

    #[test]
    fn check_add_multiples_limit_order_and_later_comsumed_by_an_market_order() {
        let mut ob = OrderBook::new();
        let mut b = buf();
        for (price, qty) in [(9, 3), (8, 5), (7, 10)] {
            b.clear();
            let o = Arc::new(Order::new(OrderType::LimitOrder, Side::Buy, price, qty));
            ob.add_order(&o, &mut b).unwrap();
        }
        b.clear();
        let market = Arc::new(Order::new(OrderType::MarketOrder, Side::Sell, 0, 10));
        ob.add_order(&market, &mut b).unwrap();
        assert_eq!(b.len(), 3);
        assert_eq!(b.as_slice().iter().map(|t| t.quantity).sum::<Quantity>(), 10);
    }

    /// 呢個 test 喺原本嘅 code 一定 panic：`cancel_order` 嘅 Sell 分支查錯 map。
    #[test]
    fn cancel_sell_order_does_not_panic() {
        let mut ob = OrderBook::new();
        let mut b = buf();
        let o = Arc::new(Order::new(OrderType::LimitOrder, Side::Sell, 100, 5));
        ob.add_order(&o, &mut b).unwrap();
        ob.cancel_order(o.order_id).unwrap();
        assert_eq!(ob.get_best_ask(), None);
        assert_eq!(ob.order_count(), 0);
    }

    /// partial fill 之後再 cancel —— 原本會 use-after-free。
    #[test]
    fn cancel_after_partial_fill() {
        let mut ob = OrderBook::new();
        let mut b = buf();

        let resting = Arc::new(Order::new(OrderType::LimitOrder, Side::Buy, 100, 10));
        ob.add_order(&resting, &mut b).unwrap();

        b.clear();
        let taker = Arc::new(Order::new(OrderType::MarketOrder, Side::Sell, 0, 4));
        ob.add_order(&taker, &mut b).unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!(b.as_slice()[0].quantity, 4);

        ob.cancel_order(resting.order_id).unwrap();
        assert_eq!(ob.get_best_bid(), None);
    }

    /// FOK：啱啱夠量應該成交，唔應該被 kill。
    #[test]
    fn fok_fills_when_liquidity_exactly_matches() {
        let mut ob = OrderBook::new();
        let mut b = buf();
        let resting = Arc::new(Order::new(OrderType::LimitOrder, Side::Sell, 100, 10));
        ob.add_order(&resting, &mut b).unwrap();

        b.clear();
        let fok = Arc::new(Order::new(OrderType::FillOrKill, Side::Buy, 100, 10));
        ob.add_order(&fok, &mut b).unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!(b.as_slice()[0].quantity, 10);
    }

    #[test]
    fn fok_is_killed_when_liquidity_insufficient() {
        let mut ob = OrderBook::new();
        let mut b = buf();
        let resting = Arc::new(Order::new(OrderType::LimitOrder, Side::Sell, 100, 5));
        ob.add_order(&resting, &mut b).unwrap();

        b.clear();
        let fok = Arc::new(Order::new(OrderType::FillOrKill, Side::Buy, 100, 10));
        ob.add_order(&fok, &mut b).unwrap();
        assert!(b.is_empty());
        assert_eq!(ob.get_best_ask(), Some(100));
    }

    #[test]
    fn ioc_takes_what_it_can_and_does_not_rest() {
        let mut ob = OrderBook::new();
        let mut b = buf();
        let resting = Arc::new(Order::new(OrderType::LimitOrder, Side::Sell, 100, 4));
        ob.add_order(&resting, &mut b).unwrap();

        b.clear();
        let ioc = Arc::new(Order::new(OrderType::ImmediateOrCancel, Side::Buy, 100, 10));
        ob.add_order(&ioc, &mut b).unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!(b.as_slice()[0].quantity, 4);
        assert_eq!(ob.get_best_bid(), None); // 餘數唔應該掛落 book
    }

    /// free list 真係被重用（原本個條件永遠唔成立）。
    #[test]
    fn price_level_slots_are_recycled() {
        let mut ob = OrderBook::new();
        let mut b = buf();
        for i in 0..64 {
            b.clear();
            let o = Arc::new(Order::new(OrderType::LimitOrder, Side::Buy, 100 + i, 1));
            ob.add_order(&o, &mut b).unwrap();
            ob.cancel_order(o.order_id).unwrap();
        }
        assert_eq!(ob.level_count(), 0);
        assert!(ob.slot_count() <= 2, "slots leaked: {}", ob.slot_count());
    }

    #[test]
    fn trade_buffer_full_is_reported() {
        let mut ob = OrderBook::new();
        let mut warm = buf();
        for _ in 0..4 {
            warm.clear();
            let o = Arc::new(Order::new(OrderType::LimitOrder, Side::Sell, 100, 1));
            ob.add_order(&o, &mut warm).unwrap();
        }
        let mut tiny = TradeBuf::with_capacity(2);
        let taker = Arc::new(Order::new(OrderType::MarketOrder, Side::Buy, 0, 4));
        assert_eq!(
            ob.add_order(&taker, &mut tiny),
            Err(OrderBookError::TradeBufferFull)
        );
    }
}

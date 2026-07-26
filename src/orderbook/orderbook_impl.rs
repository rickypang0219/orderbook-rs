use std::cmp::Reverse;
use std::collections::{BTreeMap, VecDeque};

use crate::orderbook::arena::{OrderArena, OrderSlot};
use crate::orderbook::order::{NewOrder, OrderType, Side, Status};
use crate::orderbook::price_level::{self, LevelInfo, PriceLevel};
use crate::orderbook::types::{OrderId, Price, Quantity, TradeId, NIL};

// ---------------------------------------------------------------- Trade / buf

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Trade {
    pub trade_id: TradeId,
    pub bid_order_id: OrderId,
    pub ask_order_id: OrderId,
    pub price: Price,
    pub quantity: Quantity,
    /// Taker 嘅到達序號，令下游可以還原確定性次序
    pub seq: u64,
    /// 由 caller 傳入。每個 command 讀一次 clock，唔係每筆成交讀一次。
    pub timestamp_ns: i64,
}

impl Trade {
    pub const EMPTY: Trade = Trade {
        trade_id: TradeId(0),
        bid_order_id: OrderId::INVALID,
        ask_order_id: OrderId::INVALID,
        price: 0,
        quantity: 0,
        seq: 0,
        timestamp_ns: 0,
    };
}

/// 固定容量成交輸出 buffer。Caller 持有並重用。
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
        Self::with_capacity(1024)
    }
}

// -------------------------------------------------------------------- errors

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum OrderBookError {
    #[error("order not found: {order_id}")]
    OrderNotFound { order_id: OrderId },

    #[error("invalid quantity: {quantity}")]
    InvalidQuantity { quantity: Quantity },

    #[error("order arena is full")]
    BookFull,

    #[error("price level capacity exhausted")]
    TooManyPriceLevels,

    #[error("trade buffer full")]
    TradeBufferFull,
}

// -------------------------------------------------------------------- config

#[derive(Clone, Copy, Debug)]
pub struct BookConfig {
    /// 同時 live 嘅單數上限。呢個係業務參數，唔係實作細節。
    pub max_orders: usize,
    /// 同時 live 嘅價位上限。
    pub max_levels: usize,
}

impl Default for BookConfig {
    fn default() -> Self {
        BookConfig {
            max_orders: 1 << 20, // 1,048,576 × 64 B = 64 MB
            max_levels: 1 << 12, // 4,096
        }
    }
}

// ---------------------------------------------------------------- order book

pub struct OrderBook {
    arena: OrderArena,
    /// price -> level index。原本嘅 `HashMap<OrderId, OrderEntry>` 已刪除：
    /// order lookup 而家係 `arena.get(id)`，一次 array index。
    bids: BTreeMap<Reverse<Price>, u32>,
    asks: BTreeMap<Price, u32>,
    levels: Box<[Option<PriceLevel>]>,
    free_levels: VecDeque<u32>,
    next_seq: u64,
    next_trade_id: u64,
}

impl Default for OrderBook {
    fn default() -> Self {
        Self::with_config(BookConfig::default())
    }
}

impl OrderBook {
    pub fn new() -> Self {
        Self::default()
    }

    /// **整個 engine 唯一嘅 allocation 點。**
    /// 之後 submit / cancel 唔會再掂 malloc（BTreeMap node 除外，step 7 解決）。
    pub fn with_config(cfg: BookConfig) -> Self {
        let mut free_levels = VecDeque::with_capacity(cfg.max_levels);
        for i in 0..cfg.max_levels {
            free_levels.push_back(i as u32);
        }
        OrderBook {
            arena: OrderArena::with_capacity(cfg.max_orders),
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            levels: vec![None; cfg.max_levels].into_boxed_slice(),
            free_levels,
            next_seq: 0,
            next_trade_id: 0,
        }
    }

    // ------------------------------------------------------------- level mgmt

    #[inline]
    fn level_index(&self, side: Side, price: Price) -> Option<u32> {
        match side {
            Side::Buy => self.bids.get(&Reverse(price)).copied(),
            Side::Sell => self.asks.get(&price).copied(),
        }
    }

    fn acquire_level(&mut self, price: Price) -> Result<u32, OrderBookError> {
        let i = self
            .free_levels
            .pop_front()
            .ok_or(OrderBookError::TooManyPriceLevels)?;
        self.levels[i as usize] = Some(PriceLevel::new(price));
        Ok(i)
    }

    /// `level_side` = 個 level 住喺邊一邊，唔係 taker 嘅 side。
    fn release_level(&mut self, level_side: Side, price: Price) {
        let removed = match level_side {
            Side::Buy => self.bids.remove(&Reverse(price)),
            Side::Sell => self.asks.remove(&price),
        };
        if let Some(i) = removed {
            self.levels[i as usize] = None;
            self.free_levels.push_back(i);
        }
    }

    #[inline]
    fn level_is_empty(&self, idx: u32) -> bool {
        self.levels[idx as usize]
            .as_ref()
            .map(|l| l.is_empty())
            .unwrap_or(true)
    }

    // ----------------------------------------------------------------- submit

    /// 收單。返回 engine 分配嘅 `OrderId`。
    ///
    /// * `ts_ns` 由 caller 提供：每個 command 讀一次 clock，
    ///   唔好喺每筆成交度 `Utc::now()`。
    /// * 如果張單完全成交或者唔會掛落 book（Market / IOC / 被 kill 嘅 FOK），
    ///   返回嘅 ID 已經退役 —— `get(id)` 會係 `None`，`cancel(id)` 會係
    ///   `OrderNotFound`。呢個正正係我哋想要嘅語義。
    pub fn submit(
        &mut self,
        req: &NewOrder,
        out: &mut TradeBuf,
        ts_ns: i64,
    ) -> Result<OrderId, OrderBookError> {
        if req.quantity == 0 {
            return Err(OrderBookError::InvalidQuantity {
                quantity: req.quantity,
            });
        }

        self.next_seq += 1;
        let seq = self.next_seq;

        let taker = self
            .arena
            .alloc(OrderSlot::incoming(
                req.client_order_id,
                req.order_type,
                req.side,
                req.price,
                req.quantity,
                seq,
            ))
            .ok_or(OrderBookError::BookFull)?;

        // FOK：成交前先確認夠貨。`< remaining` 而唔係 `<= original`。
        if req.order_type == OrderType::FillOrKill
            && self.available_quantity(req.side, req.price) < req.quantity
        {
            self.arena.at_mut(taker.slot()).status = Status::Canceled;
            self.arena.free(taker);
            return Ok(taker);
        }

        let is_market = req.order_type.is_market();
        let limit = if is_market {
            // Price = i64 有符號，負價係真嘢 —— 唔可以用 0 做 "sell at any price"
            match req.side {
                Side::Buy => Price::MAX,
                Side::Sell => Price::MIN,
            }
        } else {
            req.price
        };

        let traded = self.match_taker(taker, req.side, limit, is_market, seq, out, ts_ns)?;
        let remaining = req.quantity - traded;

        if remaining == 0 {
            let s = self.arena.at_mut(taker.slot());
            s.remaining_qty = 0;
            s.status = Status::Filled;
            self.arena.free(taker);
        } else if req.order_type.rests_on_book() {
            {
                let s = self.arena.at_mut(taker.slot());
                s.remaining_qty = remaining;
                s.status = if traded > 0 {
                    Status::PartiallyFilled
                } else {
                    Status::New
                };
            }
            self.rest(taker, req.side, req.price)?;
        } else {
            // Market / IOC 嘅餘數唔掛單。原本 IOC 係一個空 `=> {}`，靜靜掉單。
            let s = self.arena.at_mut(taker.slot());
            s.remaining_qty = remaining;
            s.status = Status::Canceled;
            self.arena.free(taker);
        }

        Ok(taker)
    }

    fn rest(&mut self, id: OrderId, side: Side, price: Price) -> Result<(), OrderBookError> {
        let level_idx = match self.level_index(side, price) {
            Some(i) => i,
            None => {
                let i = self.acquire_level(price)?;
                match side {
                    Side::Buy => self.bids.insert(Reverse(price), i),
                    Side::Sell => self.asks.insert(price, i),
                };
                i
            }
        };

        self.arena.at_mut(id.slot()).level = level_idx;

        // 分開 borrow 兩個 disjoint field
        let (arena, levels) = (&mut self.arena, &mut self.levels);
        let level = levels[level_idx as usize]
            .as_mut()
            .expect("level just acquired");
        price_level::push_back(arena, level, id.slot());
        Ok(())
    }

    // ----------------------------------------------------------------- cancel

    pub fn cancel(&mut self, id: OrderId) -> Result<(), OrderBookError> {
        // stale ID -> None，唔會 deref 到已釋放記憶體
        let (level_idx, side, price) = match self.arena.get(id) {
            Some(s) if s.is_resting() => (s.level, s.side, s.price),
            _ => return Err(OrderBookError::OrderNotFound { order_id: id }),
        };

        {
            let (arena, levels) = (&mut self.arena, &mut self.levels);
            let level = levels[level_idx as usize]
                .as_mut()
                .ok_or(OrderBookError::OrderNotFound { order_id: id })?;
            price_level::unlink(arena, level, id.slot());
        }

        self.arena.at_mut(id.slot()).status = Status::Canceled;
        self.arena.free(id);

        if self.level_is_empty(level_idx) {
            self.release_level(side, price);
        }
        Ok(())
    }

    // --------------------------------------------------------------- matching

    #[allow(clippy::too_many_arguments)]
    fn match_taker(
        &mut self,
        taker: OrderId,
        taker_side: Side,
        limit: Price,
        is_market: bool,
        seq: u64,
        out: &mut TradeBuf,
        ts_ns: i64,
    ) -> Result<Quantity, OrderBookError> {
        let maker_side = taker_side.opposite();
        let mut remaining = self.arena.at(taker.slot()).remaining_qty;
        let mut traded: Quantity = 0;

        while remaining > 0 {
            let best = match maker_side {
                Side::Sell => match self.asks.keys().next() {
                    Some(&p) => p,
                    None => break,
                },
                Side::Buy => match self.bids.keys().next() {
                    Some(&Reverse(p)) => p,
                    None => break,
                },
            };

            let crosses = is_market
                || match taker_side {
                    Side::Buy => limit >= best,
                    Side::Sell => limit <= best,
                };
            if !crosses {
                break;
            }

            let level_idx = match self.level_index(maker_side, best) {
                Some(i) => i,
                None => break,
            };

            let head = match self.levels[level_idx as usize].as_ref() {
                Some(l) if l.head != NIL => l.head,
                _ => {
                    self.release_level(maker_side, best);
                    continue;
                }
            };

            let maker_id = self.arena.id_at(head);
            let maker_remaining = self.arena.at(head).remaining_qty;

            // 防呆：唔應該有 0 量嘅單掛喺 book
            if maker_remaining == 0 {
                let (arena, levels) = (&mut self.arena, &mut self.levels);
                let level = levels[level_idx as usize].as_mut().expect("level exists");
                price_level::unlink(arena, level, head);
                self.arena.free(maker_id);
                if self.level_is_empty(level_idx) {
                    self.release_level(maker_side, best);
                }
                continue;
            }

            let qty = remaining.min(maker_remaining);
            let maker_full = qty == maker_remaining;

            {
                let (arena, levels) = (&mut self.arena, &mut self.levels);
                let level = levels[level_idx as usize].as_mut().expect("level exists");

                if maker_full {
                    // 先 unlink（volume 用未扣減嘅 remaining_qty），再改 slot
                    price_level::unlink(arena, level, head);
                    let s = arena.at_mut(head);
                    s.remaining_qty = 0;
                    s.status = Status::Filled;
                } else {
                    // Partial fill：**原地改一個 u64**。
                    // 冇 clone、冇 Arc::new、冇 Box::new、冇 replace_with，
                    // 所以亦都冇 pointer invalidation —— 原本嗰個 UB 消失。
                    level.volume -= qty;
                    let s = arena.at_mut(head);
                    s.remaining_qty -= qty;
                    s.status = Status::PartiallyFilled;
                }
            }

            if maker_full {
                self.arena.free(maker_id);
            }

            let (bid_order_id, ask_order_id) = match taker_side {
                Side::Buy => (taker, maker_id),
                Side::Sell => (maker_id, taker),
            };
            self.next_trade_id += 1;
            let trade = Trade {
                trade_id: TradeId(self.next_trade_id),
                bid_order_id,
                ask_order_id,
                price: best, // 成交價永遠係 maker 個價
                quantity: qty,
                seq,
                timestamp_ns: ts_ns,
            };
            if !out.push(trade) {
                return Err(OrderBookError::TradeBufferFull);
            }

            remaining -= qty;
            traded += qty;

            if self.level_is_empty(level_idx) {
                self.release_level(maker_side, best);
            }
        }

        Ok(traded)
    }

    /// Taker 可以食到幾多貨（唔 collect，零 allocation）。
    fn available_quantity(&self, taker_side: Side, limit: Price) -> Quantity {
        let levels = &self.levels;
        match taker_side {
            // Buy 食 asks，價位 <= limit
            Side::Buy => self
                .asks
                .range(..=limit)
                .filter_map(|(_, &i)| levels[i as usize].as_ref())
                .map(|l| l.volume)
                .sum(),
            // Sell 食 bids，價位 >= limit。bids 用 Reverse key，
            // 所以 `..=Reverse(p)` 展開係 `k >= p`。
            Side::Sell => self
                .bids
                .range(..=Reverse(limit))
                .filter_map(|(_, &i)| levels[i as usize].as_ref())
                .map(|l| l.volume)
                .sum(),
        }
    }

    // ------------------------------------------------------------------ query

    #[inline]
    pub fn get(&self, id: OrderId) -> Option<&OrderSlot> {
        self.arena.get(id)
    }

    #[inline]
    pub fn get_best_bid(&self) -> Option<Price> {
        self.bids.keys().next().map(|&Reverse(p)| p)
    }

    #[inline]
    pub fn get_best_ask(&self) -> Option<Price> {
        self.asks.keys().next().copied()
    }

    pub fn best_bid_level(&self) -> Option<LevelInfo> {
        let (_, &i) = self.bids.iter().next()?;
        self.levels[i as usize].as_ref().map(|l| l.info())
    }

    pub fn best_ask_level(&self) -> Option<LevelInfo> {
        let (_, &i) = self.asks.iter().next()?;
        self.levels[i as usize].as_ref().map(|l| l.info())
    }

    #[inline]
    pub fn live_orders(&self) -> u32 {
        self.arena.live()
    }

    #[inline]
    pub fn level_count(&self) -> usize {
        self.bids.len() + self.asks.len()
    }
}

// ------------------------------------------------------------------- tests

#[cfg(test)]
mod orderbook_tests {
    use super::*;
    use crate::orderbook::types::ClientOrderId;

    const TS: i64 = 1_700_000_000_000_000_000;

    fn book() -> OrderBook {
        OrderBook::with_config(BookConfig {
            max_orders: 256,
            max_levels: 64,
        })
    }

    fn buf() -> TradeBuf {
        TradeBuf::with_capacity(64)
    }

    fn limit(n: u64, side: Side, price: Price, qty: Quantity) -> NewOrder {
        NewOrder::limit(ClientOrderId(n), side, price, qty)
    }

    #[test]
    fn resting_limit_order_is_addressable_by_returned_id() {
        let (mut ob, mut b) = (book(), buf());
        let id = ob.submit(&limit(1, Side::Buy, 10, 10), &mut b, TS).unwrap();
        assert!(b.is_empty());
        assert_eq!(ob.get_best_bid(), Some(10));
        assert_eq!(ob.get(id).unwrap().remaining_qty, 10);
        assert_eq!(ob.get(id).unwrap().client_order_id, ClientOrderId(1));
    }

    #[test]
    fn market_order_consumes_the_book() {
        let (mut ob, mut b) = (book(), buf());
        ob.submit(&limit(1, Side::Buy, 10, 10), &mut b, TS).unwrap();

        b.clear();
        let taker = NewOrder::market(ClientOrderId(2), Side::Sell, 10);
        ob.submit(&taker, &mut b, TS).unwrap();

        assert_eq!(b.len(), 1);
        assert_eq!(b.as_slice()[0].price, 10);
        assert_eq!(b.as_slice()[0].quantity, 10);
        assert_eq!(ob.get_best_bid(), None);
        assert_eq!(ob.live_orders(), 0); // taker 同 maker 都還返 arena
    }

    #[test]
    fn price_time_priority_is_fifo_within_a_level() {
        let (mut ob, mut b) = (book(), buf());
        let first = ob.submit(&limit(1, Side::Buy, 10, 5), &mut b, TS).unwrap();
        let second = ob.submit(&limit(2, Side::Buy, 10, 5), &mut b, TS).unwrap();

        b.clear();
        ob.submit(
            &NewOrder::market(ClientOrderId(3), Side::Sell, 5),
            &mut b,
            TS,
        )
        .unwrap();

        assert_eq!(b.len(), 1);
        assert_eq!(b.as_slice()[0].bid_order_id, first); // 先到先食
        assert!(ob.get(first).is_none()); // 全部食晒，ID 退役
        assert_eq!(ob.get(second).unwrap().remaining_qty, 5);
    }

    #[test]
    fn best_prices_across_multiple_levels() {
        let (mut ob, mut b) = (book(), buf());
        for (i, (side, price, qty)) in [
            (Side::Buy, 9, 10),
            (Side::Buy, 8, 5),
            (Side::Buy, 7, 3),
            (Side::Sell, 10, 10),
            (Side::Sell, 11, 5),
            (Side::Sell, 12, 3),
        ]
        .into_iter()
        .enumerate()
        {
            b.clear();
            ob.submit(&limit(i as u64, side, price, qty), &mut b, TS)
                .unwrap();
        }
        assert_eq!(ob.get_best_bid(), Some(9));
        assert_eq!(ob.get_best_ask(), Some(10));
        assert_eq!(ob.level_count(), 6);
    }

    #[test]
    fn market_order_walks_multiple_levels() {
        let (mut ob, mut b) = (book(), buf());
        for (i, (price, qty)) in [(9, 3), (8, 5), (7, 10)].into_iter().enumerate() {
            b.clear();
            ob.submit(&limit(i as u64, Side::Buy, price, qty), &mut b, TS)
                .unwrap();
        }
        b.clear();
        ob.submit(
            &NewOrder::market(ClientOrderId(9), Side::Sell, 10),
            &mut b,
            TS,
        )
        .unwrap();

        assert_eq!(b.len(), 3);
        // 由最好價開始食
        assert_eq!(b.as_slice()[0].price, 9);
        assert_eq!(b.as_slice()[1].price, 8);
        assert_eq!(b.as_slice()[2].price, 7);
        assert_eq!(b.as_slice().iter().map(|t| t.quantity).sum::<Quantity>(), 10);
    }

    #[test]
    fn cancel_sell_order() {
        let (mut ob, mut b) = (book(), buf());
        let id = ob.submit(&limit(1, Side::Sell, 100, 5), &mut b, TS).unwrap();
        ob.cancel(id).unwrap();
        assert_eq!(ob.get_best_ask(), None);
        assert_eq!(ob.live_orders(), 0);
    }

    /// 原本喺呢度係 use-after-free。而家係一個 O(1) 嘅安全操作。
    #[test]
    fn cancel_after_partial_fill() {
        let (mut ob, mut b) = (book(), buf());
        let resting = ob.submit(&limit(1, Side::Buy, 100, 10), &mut b, TS).unwrap();

        b.clear();
        ob.submit(
            &NewOrder::market(ClientOrderId(2), Side::Sell, 4),
            &mut b,
            TS,
        )
        .unwrap();
        assert_eq!(b.as_slice()[0].quantity, 4);
        assert_eq!(ob.get(resting).unwrap().remaining_qty, 6);
        assert_eq!(ob.get(resting).unwrap().executed_qty(), 4);

        ob.cancel(resting).unwrap();
        assert_eq!(ob.get_best_bid(), None);
        assert!(ob.get(resting).is_none());
    }

    /// Stale ID 唔會 resolve，亦唔會撞到重用同一個 slot 嘅新單。
    #[test]
    fn stale_order_id_is_rejected_not_dereferenced() {
        let (mut ob, mut b) = (book(), buf());
        let old = ob.submit(&limit(1, Side::Buy, 100, 5), &mut b, TS).unwrap();
        ob.cancel(old).unwrap();

        assert_eq!(
            ob.cancel(old),
            Err(OrderBookError::OrderNotFound { order_id: old })
        );

        b.clear();
        let new = ob.submit(&limit(2, Side::Buy, 100, 5), &mut b, TS).unwrap();
        assert_eq!(new.slot(), old.slot()); // 同一個 slot 被重用
        assert_ne!(new, old); // 但 ID 唔同
        assert!(ob.get(old).is_none());
        assert_eq!(ob.get(new).unwrap().client_order_id, ClientOrderId(2));
    }

    #[test]
    fn fok_fills_when_liquidity_exactly_matches() {
        let (mut ob, mut b) = (book(), buf());
        ob.submit(&limit(1, Side::Sell, 100, 10), &mut b, TS).unwrap();

        b.clear();
        let fok = NewOrder {
            client_order_id: ClientOrderId(2),
            order_type: OrderType::FillOrKill,
            side: Side::Buy,
            price: 100,
            quantity: 10,
        };
        ob.submit(&fok, &mut b, TS).unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!(b.as_slice()[0].quantity, 10);
    }

    #[test]
    fn fok_is_killed_when_liquidity_insufficient() {
        let (mut ob, mut b) = (book(), buf());
        ob.submit(&limit(1, Side::Sell, 100, 5), &mut b, TS).unwrap();

        b.clear();
        let fok = NewOrder {
            client_order_id: ClientOrderId(2),
            order_type: OrderType::FillOrKill,
            side: Side::Buy,
            price: 100,
            quantity: 10,
        };
        let id = ob.submit(&fok, &mut b, TS).unwrap();
        assert!(b.is_empty());
        assert_eq!(ob.get_best_ask(), Some(100));
        assert!(ob.get(id).is_none()); // 已 kill
    }

    #[test]
    fn ioc_takes_what_it_can_and_does_not_rest() {
        let (mut ob, mut b) = (book(), buf());
        ob.submit(&limit(1, Side::Sell, 100, 4), &mut b, TS).unwrap();

        b.clear();
        let ioc = NewOrder {
            client_order_id: ClientOrderId(2),
            order_type: OrderType::ImmediateOrCancel,
            side: Side::Buy,
            price: 100,
            quantity: 10,
        };
        ob.submit(&ioc, &mut b, TS).unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!(b.as_slice()[0].quantity, 4);
        assert_eq!(ob.get_best_bid(), None); // 餘數唔掛落 book
        assert_eq!(ob.live_orders(), 0);
    }

    #[test]
    fn arena_full_is_backpressure_not_growth() {
        let mut ob = OrderBook::with_config(BookConfig {
            max_orders: 2,
            max_levels: 8,
        });
        let mut b = buf();
        ob.submit(&limit(1, Side::Buy, 10, 1), &mut b, TS).unwrap();
        ob.submit(&limit(2, Side::Buy, 11, 1), &mut b, TS).unwrap();
        assert_eq!(
            ob.submit(&limit(3, Side::Buy, 12, 1), &mut b, TS),
            Err(OrderBookError::BookFull)
        );
    }

    #[test]
    fn level_slots_are_recycled() {
        let (mut ob, mut b) = (book(), buf());
        for i in 0..1000 {
            b.clear();
            let id = ob
                .submit(&limit(i, Side::Buy, 100 + (i as Price % 8), 1), &mut b, TS)
                .unwrap();
            ob.cancel(id).unwrap();
        }
        assert_eq!(ob.level_count(), 0);
        assert_eq!(ob.live_orders(), 0);
    }

    #[test]
    fn negative_prices_are_supported() {
        let (mut ob, mut b) = (book(), buf());
        ob.submit(&limit(1, Side::Buy, -37, 5), &mut b, TS).unwrap();
        b.clear();
        // Sell market 用 Price::MIN 做 aggressive price，唔係 0
        ob.submit(
            &NewOrder::market(ClientOrderId(2), Side::Sell, 5),
            &mut b,
            TS,
        )
        .unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!(b.as_slice()[0].price, -37);
    }

    #[test]
    fn trade_ids_are_monotonic_and_distinct_from_order_ids() {
        let (mut ob, mut b) = (book(), buf());
        ob.submit(&limit(1, Side::Sell, 100, 1), &mut b, TS).unwrap();
        ob.submit(&limit(2, Side::Sell, 101, 1), &mut b, TS).unwrap();

        b.clear();
        ob.submit(
            &NewOrder::market(ClientOrderId(3), Side::Buy, 2),
            &mut b,
            TS,
        )
        .unwrap();
        assert_eq!(b.len(), 2);
        assert_eq!(b.as_slice()[0].trade_id, TradeId(1));
        assert_eq!(b.as_slice()[1].trade_id, TradeId(2));
    }
}

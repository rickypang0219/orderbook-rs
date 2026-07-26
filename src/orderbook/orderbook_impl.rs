use crate::orderbook::arena::{OrderArena, OrderSlot};
use crate::orderbook::ladder::Ladder;
use crate::orderbook::mem;
use crate::orderbook::order::{NewOrder, OrderType, Side, Status};
use crate::orderbook::price_level::LevelInfo;
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

    #[error("price {price} is outside the ladder band or not on a tick boundary")]
    PriceNotOnLadder { price: Price },

    #[error("order arena is full")]
    BookFull,

    #[error("trade buffer full")]
    TradeBufferFull,
}

// -------------------------------------------------------------------- config

#[derive(Clone, Copy, Debug)]
pub struct BookConfig {
    /// 同時 live 嘅單數上限（arena slot 數）。
    pub max_orders: usize,
    /// Ladder tick 0 對應嘅價。
    pub base_price: Price,
    /// 最小報價單位。
    pub tick_size: Price,
    /// Ladder 有幾多格。價格帶 = [base, base + tick*(n-1)]。
    pub num_ticks: usize,
}

impl Default for BookConfig {
    fn default() -> Self {
        BookConfig {
            max_orders: 1 << 20, // 1,048,576 × 64 B = 64 MB
            base_price: 0,
            tick_size: 1,
            num_ticks: 1 << 16, // 65,536 × 24 B × 2 sides ≈ 3.1 MB
        }
    }
}

// ---------------------------------------------------------------- order book

pub struct OrderBook {
    arena: OrderArena,
    bids: Ladder,
    asks: Ladder,
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
    /// 之後 submit / cancel 一個 malloc 都唔會做。
    pub fn with_config(cfg: BookConfig) -> Self {
        OrderBook {
            arena: OrderArena::with_capacity(cfg.max_orders),
            bids: Ladder::new(cfg.base_price, cfg.tick_size, cfg.num_ticks),
            asks: Ladder::new(cfg.base_price, cfg.tick_size, cfg.num_ticks),
            next_seq: 0,
            next_trade_id: 0,
        }
    }

    /// Step 8：pre-fault + mlock + hugepage。
    ///
    /// 喺開始收流量之前叫一次。冇佢嘅話頭幾千張單會食晒 page fault，
    /// warm-up 期間嘅 p99 會好難睇。
    pub fn warm_up(&mut self) -> mem::MemReport {
        let a = mem::warm(self.arena.slots_mut());
        let b = mem::warm(self.bids.levels_mut());
        let c = mem::warm(self.asks.levels_mut());
        mem::MemReport {
            bytes_prefaulted: a.bytes_prefaulted + b.bytes_prefaulted + c.bytes_prefaulted,
            locked: a.locked && b.locked && c.locked,
            hugepage_advised: a.hugepage_advised,
        }
    }

    // ----------------------------------------------------------------- submit

    /// 收單。返回 engine 分配嘅 `OrderId`。
    ///
    /// `ts_ns` 由 caller 提供：每個 command 讀一次 clock。
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

        // 限價單一定要落格兼喺 band 內。喺 arena.alloc 之前先驗，
        // 咁 reject 路徑就完全唔會郁到 arena。
        let is_market = req.order_type.is_market();
        if !is_market && self.asks.tick_of(req.price).is_none() {
            return Err(OrderBookError::PriceNotOnLadder { price: req.price });
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

        // FOK：成交前確認夠貨。`< quantity`，唔係 `<= original`。
        if req.order_type == OrderType::FillOrKill
            && self.available_quantity(req.side, req.price) < req.quantity
        {
            self.arena.at_mut(taker.slot()).status = Status::Canceled;
            self.arena.free(taker);
            return Ok(taker);
        }

        let traded = self.match_taker(taker, req.side, req.price, is_market, seq, out, ts_ns)?;
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
            // Market / IOC 嘅餘數唔掛單
            let s = self.arena.at_mut(taker.slot());
            s.remaining_qty = remaining;
            s.status = Status::Canceled;
            self.arena.free(taker);
        }

        Ok(taker)
    }

    fn rest(&mut self, id: OrderId, side: Side, price: Price) -> Result<(), OrderBookError> {
        // 拆開 disjoint field borrow
        let OrderBook {
            arena, bids, asks, ..
        } = self;
        let ladder = match side {
            Side::Buy => bids,
            Side::Sell => asks,
        };
        let tick = ladder
            .tick_of(price)
            .ok_or(OrderBookError::PriceNotOnLadder { price })?;

        // `level` 而家存嘅係 tick，唔再係一個要回收嘅 slot index
        arena.at_mut(id.slot()).level = tick;
        ladder.push_back(arena, tick, id.slot());
        Ok(())
    }

    // ----------------------------------------------------------------- cancel

    pub fn cancel(&mut self, id: OrderId) -> Result<(), OrderBookError> {
        let (tick, side) = match self.arena.get(id) {
            Some(s) if s.is_resting() => (s.level, s.side),
            _ => return Err(OrderBookError::OrderNotFound { order_id: id }),
        };

        let OrderBook {
            arena, bids, asks, ..
        } = self;
        let ladder = match side {
            Side::Buy => bids,
            Side::Sell => asks,
        };

        ladder.unlink(arena, tick, id.slot());
        arena.at_mut(id.slot()).status = Status::Canceled;
        arena.free(id);
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
        let OrderBook {
            arena,
            bids,
            asks,
            next_trade_id,
            ..
        } = self;

        let maker_side = taker_side.opposite();
        let ladder: &mut Ladder = match maker_side {
            Side::Buy => bids,
            Side::Sell => asks,
        };

        let mut remaining = arena.at(taker.slot()).remaining_qty;
        let mut traded: Quantity = 0;

        while remaining > 0 {
            // O(depth) 搵 best price —— 三條 bit-scan 指令，唔使行 tree
            let best_tick = match maker_side {
                Side::Sell => ladder.lowest_tick(),  // asks：價越低越好
                Side::Buy => ladder.highest_tick(),  // bids：價越高越好
            };
            let Some(best_tick) = best_tick else { break };
            let best_price = ladder.price_of(best_tick);

            let crosses = is_market
                || match taker_side {
                    Side::Buy => limit >= best_price,
                    Side::Sell => limit <= best_price,
                };
            if !crosses {
                break;
            }

            let head = ladder.level(best_tick).head;
            if head == NIL {
                // 防呆：occupancy 同鏈唔同步（唔應該發生）
                ladder.force_clear(best_tick);
                continue;
            }

            let maker_id = arena.id_at(head);
            let maker_remaining = arena.at(head).remaining_qty;
            if maker_remaining == 0 {
                ladder.unlink(arena, best_tick, head);
                arena.free(maker_id);
                continue;
            }

            let qty = remaining.min(maker_remaining);

            if qty == maker_remaining {
                // Full fill：先 unlink（volume 用未扣減嘅數），再改 slot，最後還 arena
                ladder.unlink(arena, best_tick, head);
                let s = arena.at_mut(head);
                s.remaining_qty = 0;
                s.status = Status::Filled;
                arena.free(maker_id);
            } else {
                // Partial fill：原地改兩個數。冇 node 移動、冇 allocation。
                ladder.reduce_volume(best_tick, qty);
                let s = arena.at_mut(head);
                s.remaining_qty -= qty;
                s.status = Status::PartiallyFilled;
            }

            let (bid_order_id, ask_order_id) = match taker_side {
                Side::Buy => (taker, maker_id),
                Side::Sell => (maker_id, taker),
            };
            *next_trade_id += 1;
            let trade = Trade {
                trade_id: TradeId(*next_trade_id),
                bid_order_id,
                ask_order_id,
                price: best_price, // 成交價永遠係 maker 個價
                quantity: qty,
                seq,
                timestamp_ns: ts_ns,
            };
            if !out.push(trade) {
                return Err(OrderBookError::TradeBufferFull);
            }

            remaining -= qty;
            traded += qty;
        }

        Ok(traded)
    }

    /// Taker 喺 `limit` 之內可以食到幾多貨。零 allocation，只行 occupied tick。
    fn available_quantity(&self, taker_side: Side, limit: Price) -> Quantity {
        match taker_side {
            // Buy 食 asks，由最低價一路上到 limit
            Side::Buy => match self.asks.tick_floor(limit) {
                Some(hi) => self.asks.volume_between(0, hi),
                None => 0,
            },
            // Sell 食 bids，由 limit 一路上到最高價
            Side::Sell => match self.bids.tick_ceil(limit) {
                Some(lo) => self.bids.volume_between(lo, self.bids.num_ticks() as u32 - 1),
                None => 0,
            },
        }
    }

    // ------------------------------------------------------------------ query

    #[inline]
    pub fn get(&self, id: OrderId) -> Option<&OrderSlot> {
        self.arena.get(id)
    }

    #[inline]
    pub fn get_best_bid(&self) -> Option<Price> {
        self.bids.highest_tick().map(|t| self.bids.price_of(t))
    }

    #[inline]
    pub fn get_best_ask(&self) -> Option<Price> {
        self.asks.lowest_tick().map(|t| self.asks.price_of(t))
    }

    pub fn best_bid_level(&self) -> Option<LevelInfo> {
        self.bids.highest_tick().map(|t| self.bids.level_info(t))
    }

    pub fn best_ask_level(&self) -> Option<LevelInfo> {
        self.asks.lowest_tick().map(|t| self.asks.level_info(t))
    }

    /// 由 best bid 向下走 `depth` 個 occupied level，寫入 `out`。
    /// `out` 由 caller 提供並重用 —— market data snapshot 都唔應該 allocate。
    pub fn bid_depth(&self, out: &mut [LevelInfo]) -> usize {
        let mut n = 0;
        let mut tick = self.bids.highest_tick();
        while let (Some(t), true) = (tick, n < out.len()) {
            out[n] = self.bids.level_info(t);
            n += 1;
            tick = if t == 0 {
                None
            } else {
                self.bids.next_occupied_below(t - 1)
            };
        }
        n
    }

    pub fn ask_depth(&self, out: &mut [LevelInfo]) -> usize {
        let mut n = 0;
        let mut tick = self.asks.lowest_tick();
        while let (Some(t), true) = (tick, n < out.len()) {
            out[n] = self.asks.level_info(t);
            n += 1;
            tick = self.asks.next_occupied_above(t + 1);
        }
        n
    }

    #[inline]
    pub fn live_orders(&self) -> u32 {
        self.arena.live()
    }

    #[inline]
    pub fn level_count(&self) -> u32 {
        self.bids.live_levels() + self.asks.live_levels()
    }
}

// ------------------------------------------------------------------- tests

#[cfg(test)]
mod orderbook_tests {
    use super::*;
    use crate::orderbook::types::ClientOrderId;

    const TS: i64 = 1_700_000_000_000_000_000;

    /// band = [-100, 923]，tick 1
    fn book() -> OrderBook {
        OrderBook::with_config(BookConfig {
            max_orders: 256,
            base_price: -100,
            tick_size: 1,
            num_ticks: 1024,
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
        ob.submit(
            &NewOrder::market(ClientOrderId(2), Side::Sell, 10),
            &mut b,
            TS,
        )
        .unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!(b.as_slice()[0].price, 10);
        assert_eq!(ob.get_best_bid(), None);
        assert_eq!(ob.live_orders(), 0);
        assert_eq!(ob.level_count(), 0);
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
        assert_eq!(b.as_slice()[0].bid_order_id, first);
        assert!(ob.get(first).is_none());
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
    fn market_order_walks_multiple_levels_best_first() {
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
        assert_eq!(ob.level_count(), 0);
    }

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
        assert_eq!(ob.get(resting).unwrap().remaining_qty, 6);
        assert_eq!(ob.get(resting).unwrap().executed_qty(), 4);
        assert_eq!(ob.best_bid_level().unwrap().volume, 6);
        ob.cancel(resting).unwrap();
        assert_eq!(ob.get_best_bid(), None);
        assert!(ob.get(resting).is_none());
    }

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
        assert_eq!(new.slot(), old.slot());
        assert_ne!(new, old);
        assert!(ob.get(old).is_none());
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
        assert!(ob.get(id).is_none());
    }

    #[test]
    fn fok_sell_side_uses_the_bid_ladder() {
        let (mut ob, mut b) = (book(), buf());
        ob.submit(&limit(1, Side::Buy, 100, 6), &mut b, TS).unwrap();
        ob.submit(&limit(2, Side::Buy, 99, 6), &mut b, TS).unwrap();
        b.clear();
        // limit 99 -> 兩個價位都夠得着，總量 12 >= 10
        let fok = NewOrder {
            client_order_id: ClientOrderId(3),
            order_type: OrderType::FillOrKill,
            side: Side::Sell,
            price: 99,
            quantity: 10,
        };
        ob.submit(&fok, &mut b, TS).unwrap();
        assert_eq!(b.as_slice().iter().map(|t| t.quantity).sum::<Quantity>(), 10);
        assert_eq!(b.as_slice()[0].price, 100); // 由最好價開始
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
        assert_eq!(b.as_slice()[0].quantity, 4);
        assert_eq!(ob.get_best_bid(), None);
        assert_eq!(ob.live_orders(), 0);
    }

    #[test]
    fn negative_prices_work() {
        let (mut ob, mut b) = (book(), buf());
        ob.submit(&limit(1, Side::Buy, -37, 5), &mut b, TS).unwrap();
        assert_eq!(ob.get_best_bid(), Some(-37));
        b.clear();
        ob.submit(
            &NewOrder::market(ClientOrderId(2), Side::Sell, 5),
            &mut b,
            TS,
        )
        .unwrap();
        assert_eq!(b.as_slice()[0].price, -37);
    }

    #[test]
    fn price_outside_band_is_rejected_before_touching_the_arena() {
        let (mut ob, mut b) = (book(), buf());
        assert_eq!(
            ob.submit(&limit(1, Side::Buy, 100_000, 1), &mut b, TS),
            Err(OrderBookError::PriceNotOnLadder { price: 100_000 })
        );
        assert_eq!(ob.live_orders(), 0);
        assert_eq!(ob.get(OrderId::new(0, 1)), None);
    }

    #[test]
    fn off_tick_price_is_rejected() {
        let mut ob = OrderBook::with_config(BookConfig {
            max_orders: 16,
            base_price: 1000,
            tick_size: 25,
            num_ticks: 100,
        });
        let mut b = buf();
        assert!(ob.submit(&limit(1, Side::Buy, 1025, 1), &mut b, TS).is_ok());
        assert_eq!(
            ob.submit(&limit(2, Side::Buy, 1010, 1), &mut b, TS),
            Err(OrderBookError::PriceNotOnLadder { price: 1010 })
        );
    }

    #[test]
    fn arena_full_is_backpressure_not_growth() {
        let mut ob = OrderBook::with_config(BookConfig {
            max_orders: 2,
            base_price: 0,
            tick_size: 1,
            num_ticks: 64,
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
    fn depth_snapshot_needs_no_allocation() {
        let (mut ob, mut b) = (book(), buf());
        for (i, price) in [9, 8, 7, 6].into_iter().enumerate() {
            b.clear();
            ob.submit(&limit(i as u64, Side::Buy, price, 10), &mut b, TS)
                .unwrap();
        }
        let mut depth = [LevelInfo {
            price: 0,
            volume: 0,
            order_count: 0,
        }; 3];
        let n = ob.bid_depth(&mut depth);
        assert_eq!(n, 3);
        assert_eq!(depth[0].price, 9);
        assert_eq!(depth[1].price, 8);
        assert_eq!(depth[2].price, 7);
    }

    #[test]
    fn churn_does_not_leak_levels_or_slots() {
        let (mut ob, mut b) = (book(), buf());
        for i in 0..5000u64 {
            b.clear();
            let id = ob
                .submit(&limit(i, Side::Buy, (i % 8) as Price, 1), &mut b, TS)
                .unwrap();
            ob.cancel(id).unwrap();
        }
        assert_eq!(ob.level_count(), 0);
        assert_eq!(ob.live_orders(), 0);
        assert_eq!(ob.get_best_bid(), None);
    }

    #[test]
    fn trade_ids_are_monotonic() {
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
        assert_eq!(b.as_slice()[0].trade_id, TradeId(1));
        assert_eq!(b.as_slice()[1].trade_id, TradeId(2));
    }
}

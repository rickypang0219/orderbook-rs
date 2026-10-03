use crate::orderbook::arena::{OrderArena, OrderSlot};
use crate::orderbook::ladder::Ladder;
use crate::orderbook::mem;
use crate::orderbook::order::{NewOrder, OrderType, Side, Status};
use crate::orderbook::price_level::LevelInfo;
use crate::orderbook::types::{NIL, OrderId, Price, Quantity, TradeId};

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

    #[error("quantity or price-level volume overflow")]
    QuantityOverflow,

    #[error("sequence or trade ID exhausted")]
    IdExhausted,
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

    /// Accept an order. On Err, both the book and output buffer are unchanged.
    /// A killed FOK returns an ID which is already inactive.
    pub fn submit(
        &mut self,
        req: &NewOrder,
        out: &mut TradeBuf,
        ts_ns: i64,
    ) -> Result<OrderId, OrderBookError> {
        let execute = self.preflight(req, out, None)?;
        let seq = self.next_seq + 1;
        let id = self
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
        self.next_seq = seq;
        if execute {
            self.execute(id, req, out, ts_ns);
        } else {
            self.arena.free(id);
        }
        Ok(id)
    }

    /// Set a resting order's price and remaining quantity, preserving its ID.
    /// Same-price reductions keep FIFO priority; increases/reprices lose it.
    /// Zero cancels (price is ignored). Executed quantity is preserved.
    /// On Err, the original order and output buffer are unchanged.
    pub fn amend(
        &mut self,
        id: OrderId,
        price: Price,
        remaining: Quantity,
        out: &mut TradeBuf,
        ts_ns: i64,
    ) -> Result<(), OrderBookError> {
        let old = *self
            .arena
            .get(id)
            .filter(|s| s.is_resting())
            .ok_or(OrderBookError::OrderNotFound { order_id: id })?;
        if remaining == 0 {
            return self.cancel(id);
        }
        let original = old
            .executed_qty()
            .checked_add(remaining)
            .ok_or(OrderBookError::QuantityOverflow)?;
        if price == old.price && remaining <= old.remaining_qty {
            let ladder = match old.side {
                Side::Buy => &mut self.bids,
                Side::Sell => &mut self.asks,
            };
            ladder.reduce_volume(old.level, old.remaining_qty - remaining);
            let slot = self.arena.at_mut(id.slot());
            slot.remaining_qty = remaining;
            slot.original_qty = original;
            return Ok(());
        }
        let req = NewOrder {
            client_order_id: old.client_order_id,
            order_type: old.order_type,
            side: old.side,
            price,
            quantity: remaining,
        };
        self.preflight(&req, out, Some(&old))?;
        self.unlink(id);
        self.next_seq += 1;
        let slot = self.arena.at_mut(id.slot());
        slot.price = price;
        slot.remaining_qty = remaining;
        slot.original_qty = original;
        slot.seq = self.next_seq;
        self.execute(id, &req, out, ts_ns);
        Ok(())
    }

    pub fn cancel(&mut self, id: OrderId) -> Result<(), OrderBookError> {
        if !self.arena.get(id).is_some_and(|s| s.is_resting()) {
            return Err(OrderBookError::OrderNotFound { order_id: id });
        }
        self.unlink(id);
        self.arena.free(id);
        Ok(())
    }

    fn ladder(&self, side: Side) -> &Ladder {
        match side {
            Side::Buy => &self.bids,
            Side::Sell => &self.asks,
        }
    }

    fn crosses(req: &NewOrder, price: Price) -> bool {
        req.order_type.is_market()
            || match req.side {
                Side::Buy => req.price >= price,
                Side::Sell => req.price <= price,
            }
    }

    /// Validate all fallible work before touching orders or emitting trades.
    /// Usually no maker scan is needed: each fill consumes >= 1 quantity and
    /// one live maker, so min(quantity, live orders) bounds the number of fills.
    fn preflight(
        &self,
        req: &NewOrder,
        out: &TradeBuf,
        replaced: Option<&OrderSlot>,
    ) -> Result<bool, OrderBookError> {
        if req.quantity == 0 {
            return Err(OrderBookError::InvalidQuantity { quantity: 0 });
        }
        let own = self.ladder(req.side);
        let tick = own.tick_of(req.price);
        if !req.order_type.is_market() && tick.is_none() {
            return Err(OrderBookError::PriceNotOnLadder { price: req.price });
        }
        if self.next_seq == u64::MAX {
            return Err(OrderBookError::IdExhausted);
        }
        if req.order_type == OrderType::FillOrKill && !self.has_liquidity(req) {
            return Ok(false);
        }
        let volume = if req.order_type.rests_on_book() {
            let tick = tick.expect("validated limit price");
            own.level(tick).volume
                - replaced
                    .filter(|s| s.level == tick)
                    .map_or(0, |s| s.remaining_qty)
        } else {
            0
        };
        let fills_bound = req.quantity.min(u64::from(self.arena.live()));
        let room = (out.capacity() - out.len()) as u64;
        let ids_left = u64::MAX - self.next_trade_id;
        if fills_bound <= room
            && fills_bound <= ids_left
            && volume.checked_add(req.quantity).is_some()
        {
            return Ok(true);
        }

        let maker_side = req.side.opposite();
        let ladder = self.ladder(maker_side);
        let mut tick = ladder.best_tick(maker_side);
        let mut remaining = req.quantity;
        let mut fills = 0;
        while let Some(t) = tick {
            if remaining == 0 || !Self::crosses(req, ladder.price_of(t)) {
                break;
            }
            let mut slot = ladder.level(t).head;
            while slot != NIL && remaining > 0 {
                fills += 1;
                if fills > room {
                    return Err(OrderBookError::TradeBufferFull);
                }
                if fills > ids_left {
                    return Err(OrderBookError::IdExhausted);
                }
                let maker = self.arena.at(slot);
                remaining -= remaining.min(maker.remaining_qty);
                slot = maker.next;
            }
            tick = ladder.next_tick(maker_side, t);
        }
        if req.order_type.rests_on_book() && volume.checked_add(remaining).is_none() {
            return Err(OrderBookError::QuantityOverflow);
        }
        Ok(true)
    }

    /// Stop as soon as enough liquidity is found; subtraction cannot overflow.
    fn has_liquidity(&self, req: &NewOrder) -> bool {
        let side = req.side.opposite();
        let ladder = self.ladder(side);
        let mut tick = ladder.best_tick(side);
        let mut needed = req.quantity;
        while let Some(t) = tick {
            if !Self::crosses(req, ladder.price_of(t)) {
                break;
            }
            let volume = ladder.level(t).volume;
            if volume >= needed {
                return true;
            }
            needed -= volume;
            tick = ladder.next_tick(side, t);
        }
        false
    }

    fn unlink(&mut self, id: OrderId) {
        let slot = self.arena.at(id.slot());
        let tick = slot.level;
        let ladder = match slot.side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        };
        ladder.unlink(&mut self.arena, tick, id.slot());
    }

    /// Preflight guarantees capacity and arithmetic bounds; no recoverable errors here.
    fn execute(&mut self, id: OrderId, req: &NewOrder, out: &mut TradeBuf, ts_ns: i64) {
        let traded = self.match_taker(id, req, out, ts_ns);
        let slot = self.arena.at_mut(id.slot());
        slot.remaining_qty -= traded;
        if slot.remaining_qty == 0 || !req.order_type.rests_on_book() {
            self.arena.free(id);
            return;
        }
        slot.status = if slot.executed_qty() > 0 {
            Status::PartiallyFilled
        } else {
            Status::New
        };
        let ladder = match req.side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        };
        let tick = ladder.tick_of(req.price).expect("validated limit price");
        slot.level = tick;
        ladder.push_back(&mut self.arena, tick, id.slot());
    }

    fn match_taker(
        &mut self,
        taker: OrderId,
        req: &NewOrder,
        out: &mut TradeBuf,
        ts_ns: i64,
    ) -> Quantity {
        let maker_side = req.side.opposite();
        let ladder = match maker_side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        };
        let arena = &mut self.arena;
        let seq = arena.at(taker.slot()).seq;
        let mut remaining = req.quantity;
        while remaining > 0 {
            let Some(tick) = ladder.best_tick(maker_side) else {
                break;
            };
            let price = ladder.price_of(tick);
            if !Self::crosses(req, price) {
                break;
            }
            // Drain this FIFO before searching the bitmap again.
            while remaining > 0 && !ladder.level(tick).is_empty() {
                let head = ladder.level(tick).head;
                let maker = arena.id_at(head);
                let qty = remaining.min(arena.at(head).remaining_qty);
                debug_assert!(qty > 0);
                if qty == arena.at(head).remaining_qty {
                    ladder.unlink(arena, tick, head);
                    arena.free(maker);
                } else {
                    ladder.reduce_volume(tick, qty);
                    let slot = arena.at_mut(head);
                    slot.remaining_qty -= qty;
                    slot.status = Status::PartiallyFilled;
                }
                let (bid_order_id, ask_order_id) = match req.side {
                    Side::Buy => (taker, maker),
                    Side::Sell => (maker, taker),
                };
                self.next_trade_id += 1;
                assert!(
                    out.push(Trade {
                        trade_id: TradeId(self.next_trade_id),
                        bid_order_id,
                        ask_order_id,
                        price,
                        quantity: qty,
                        seq,
                        timestamp_ns: ts_ns,
                    }),
                    "preflight reserved trade capacity"
                );
                remaining -= qty;
            }
        }
        req.quantity - remaining
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
    fn exhausted_counters_reject_before_mutation() {
        let (mut ob, mut out) = (book(), buf());
        let id = ob
            .submit(&limit(1, Side::Sell, 100, 2), &mut out, TS)
            .unwrap();
        let before = *ob.get(id).unwrap();
        ob.next_trade_id = u64::MAX;
        assert_eq!(
            ob.submit(
                &NewOrder::market(ClientOrderId(2), Side::Buy, 1),
                &mut out,
                TS
            ),
            Err(OrderBookError::IdExhausted)
        );
        assert_eq!(*ob.get(id).unwrap(), before);
        assert!(out.is_empty());
        ob.next_seq = u64::MAX;
        assert_eq!(
            ob.submit(&limit(3, Side::Buy, 99, 1), &mut out, TS),
            Err(OrderBookError::IdExhausted)
        );
        assert_eq!(
            ob.amend(id, 101, 2, &mut out, TS),
            Err(OrderBookError::IdExhausted)
        );
        assert_eq!(*ob.get(id).unwrap(), before);
        ob.cancel(id).unwrap();
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
        assert_eq!(
            b.as_slice().iter().map(|t| t.quantity).sum::<Quantity>(),
            10
        );
    }

    #[test]
    fn cancel_sell_order() {
        let (mut ob, mut b) = (book(), buf());
        let id = ob
            .submit(&limit(1, Side::Sell, 100, 5), &mut b, TS)
            .unwrap();
        ob.cancel(id).unwrap();
        assert_eq!(ob.get_best_ask(), None);
        assert_eq!(ob.live_orders(), 0);
        assert_eq!(ob.level_count(), 0);
    }

    #[test]
    fn cancel_after_partial_fill() {
        let (mut ob, mut b) = (book(), buf());
        let resting = ob
            .submit(&limit(1, Side::Buy, 100, 10), &mut b, TS)
            .unwrap();
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
        ob.submit(&limit(1, Side::Sell, 100, 10), &mut b, TS)
            .unwrap();
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
        ob.submit(&limit(1, Side::Sell, 100, 5), &mut b, TS)
            .unwrap();
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
        assert_eq!(
            b.as_slice().iter().map(|t| t.quantity).sum::<Quantity>(),
            10
        );
        assert_eq!(b.as_slice()[0].price, 100); // 由最好價開始
    }

    #[test]
    fn ioc_takes_what_it_can_and_does_not_rest() {
        let (mut ob, mut b) = (book(), buf());
        ob.submit(&limit(1, Side::Sell, 100, 4), &mut b, TS)
            .unwrap();
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
        ob.submit(&limit(1, Side::Sell, 100, 1), &mut b, TS)
            .unwrap();
        ob.submit(&limit(2, Side::Sell, 101, 1), &mut b, TS)
            .unwrap();
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

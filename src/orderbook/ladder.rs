//! 固定 tick grid。
//!
//! `BTreeMap<Price, PriceLevelRef>` 有兩個問題：
//!   1. 每個 node 係一次 `Box` allocation（滿咗仲要 split，再 alloc）
//!   2. `iter().next()` 攞 best price 要行返 tree 最左路徑，O(log n)
//!
//! Crypto perp / futures 嘅 tick grid 係固定嘅，所以 price -> index 可以直接
//! 用算術做。整個 grid 喺 init 一次過分配好，之後零 allocation，
//! best price 靠 occupancy bitmap 一次 O(depth) 查詢。
//!
//! 代價：價格帶 (band) 變成有界。band 外嘅價會被 reject —— 呢個對交易所嚟講
//! 本來就係應有行為（price banding / limit up-down），唔係限制。

use crate::orderbook::arena::OrderArena;
use crate::orderbook::bitset::HierBitset;
use crate::orderbook::price_level::{self, LevelInfo, PriceLevel};
use crate::orderbook::types::{Price, Quantity};

pub struct Ladder {
    base_price: Price,
    tick_size: Price,
    levels: Box<[PriceLevel]>,
    occupancy: HierBitset,
    live_levels: u32,
}

impl Ladder {
    pub fn new(base_price: Price, tick_size: Price, num_ticks: usize) -> Self {
        assert!(tick_size > 0, "tick_size must be positive");
        assert!(num_ticks > 0, "num_ticks must be positive");
        Ladder {
            base_price,
            tick_size,
            levels: vec![PriceLevel::EMPTY; num_ticks].into_boxed_slice(),
            occupancy: HierBitset::with_capacity(num_ticks),
            live_levels: 0,
        }
    }

    #[inline(always)]
    pub fn num_ticks(&self) -> usize {
        self.levels.len()
    }

    #[inline(always)]
    pub fn live_levels(&self) -> u32 {
        self.live_levels
    }

    #[inline(always)]
    pub fn min_price(&self) -> Price {
        self.base_price
    }

    #[inline(always)]
    pub fn max_price(&self) -> Price {
        self.base_price + (self.levels.len() as Price - 1) * self.tick_size
    }

    /// 精確落格嘅價 -> tick。唔喺 grid 上或者出 band 都返 `None`。
    #[inline]
    pub fn tick_of(&self, price: Price) -> Option<u32> {
        if price < self.base_price {
            return None;
        }
        let d = price - self.base_price;
        if d % self.tick_size != 0 {
            return None;
        }
        let t = d / self.tick_size;
        (t < self.levels.len() as Price).then_some(t as u32)
    }

    #[inline(always)]
    pub fn price_of(&self, tick: u32) -> Price {
        self.base_price + (tick as Price) * self.tick_size
    }

    /// <= `price` 嘅最大 tick，clamp 到 band 頂。`None` = price 低過成個 band。
    #[inline]
    pub fn tick_floor(&self, price: Price) -> Option<u32> {
        if price < self.base_price {
            return None;
        }
        let t = (price - self.base_price) / self.tick_size;
        Some(t.min(self.levels.len() as Price - 1) as u32)
    }

    /// >= `price` 嘅最小 tick，clamp 到 band 底。`None` = price 高過成個 band。
    #[inline]
    pub fn tick_ceil(&self, price: Price) -> Option<u32> {
        if price <= self.base_price {
            return Some(0);
        }
        let d = price - self.base_price;
        let t = d.div_euclid(self.tick_size) + i64::from(d % self.tick_size != 0);
        (t < self.levels.len() as Price).then_some(t as u32)
    }

    // ------------------------------------------------------------- best price

    /// 最低有單嘅 tick（asks 側 = best ask）
    #[inline]
    pub fn lowest_tick(&self) -> Option<u32> {
        self.occupancy.lowest().map(|t| t as u32)
    }

    /// 最高有單嘅 tick（bids 側 = best bid）
    #[inline]
    pub fn highest_tick(&self) -> Option<u32> {
        self.occupancy.highest().map(|t| t as u32)
    }

    #[inline]
    pub fn next_occupied_above(&self, tick: u32) -> Option<u32> {
        self.occupancy.next_at_or_above(tick as usize).map(|t| t as u32)
    }

    #[inline]
    pub fn next_occupied_below(&self, tick: u32) -> Option<u32> {
        self.occupancy.next_at_or_below(tick as usize).map(|t| t as u32)
    }

    // ----------------------------------------------------------------- access

    #[inline(always)]
    pub fn level(&self, tick: u32) -> &PriceLevel {
        &self.levels[tick as usize]
    }

    /// 俾 `mem::warm` 用。
    #[inline]
    pub fn levels_mut(&mut self) -> &mut [PriceLevel] {
        &mut self.levels
    }

    #[inline]
    pub fn level_info(&self, tick: u32) -> LevelInfo {
        let l = &self.levels[tick as usize];
        LevelInfo {
            price: self.price_of(tick),
            volume: l.volume,
            order_count: l.order_count,
        }
    }

    // ---------------------------------------------------------------- mutate

    /// 掛單。level 由空變非空嘅時候順手 set occupancy bit。
    pub fn push_back(&mut self, arena: &mut OrderArena, tick: u32, slot: u32) {
        let became_occupied = {
            let level = &mut self.levels[tick as usize];
            let was_empty = level.is_empty();
            price_level::push_back(arena, level, slot);
            was_empty
        };
        if became_occupied {
            self.occupancy.insert(tick as usize);
            self.live_levels += 1;
        }
    }

    /// 摘單。level 變空就順手 clear occupancy bit。
    ///
    /// 對比之前：唔再需要 free-list、唔再需要 `Option<PriceLevel>`、
    /// 唔再需要由 `bids`/`asks` map 度 remove。整個 level 回收機制消失。
    pub fn unlink(&mut self, arena: &mut OrderArena, tick: u32, slot: u32) {
        let became_empty = {
            let level = &mut self.levels[tick as usize];
            price_level::unlink(arena, level, slot);
            level.is_empty()
        };
        if became_empty {
            self.occupancy.remove(tick as usize);
            self.live_levels -= 1;
        }
    }

    /// Partial fill：原地扣減 volume，唔郁鏈。
    #[inline]
    pub fn reduce_volume(&mut self, tick: u32, qty: Quantity) {
        self.levels[tick as usize].volume -= qty;
    }

    /// 防呆：強制把一個 tick 標成空（唔應該用到）。
    pub fn force_clear(&mut self, tick: u32) {
        if self.occupancy.contains(tick as usize) {
            self.occupancy.remove(tick as usize);
            self.live_levels -= 1;
        }
        self.levels[tick as usize] = PriceLevel::EMPTY;
    }

    /// 由 `lo` 到 `hi`（含兩端）加總所有 occupied level 嘅 volume。零 allocation。
    pub fn volume_between(&self, lo: u32, hi: u32) -> Quantity {
        if lo > hi {
            return 0;
        }
        let mut total: Quantity = 0;
        let mut cur = self.occupancy.next_at_or_above(lo as usize);
        while let Some(t) = cur {
            if t > hi as usize {
                break;
            }
            total += self.levels[t].volume;
            cur = self.occupancy.next_at_or_above(t + 1);
        }
        total
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orderbook::arena::OrderSlot;
    use crate::orderbook::order::{OrderType, Side};
    use crate::orderbook::types::ClientOrderId;

    fn ladder() -> Ladder {
        // band = [100, 100 + 255] with tick 1
        Ladder::new(100, 1, 256)
    }

    fn slot(qty: Quantity) -> OrderSlot {
        OrderSlot::incoming(ClientOrderId(0), OrderType::LimitOrder, Side::Buy, 0, qty, 0)
    }

    #[test]
    fn price_tick_roundtrip() {
        let l = Ladder::new(1000, 25, 100);
        assert_eq!(l.tick_of(1000), Some(0));
        assert_eq!(l.tick_of(1025), Some(1));
        assert_eq!(l.tick_of(1010), None); // 唔落格
        assert_eq!(l.tick_of(999), None); // 出 band
        assert_eq!(l.tick_of(1000 + 25 * 100), None); // 出 band
        assert_eq!(l.price_of(4), 1100);
        assert_eq!(l.min_price(), 1000);
        assert_eq!(l.max_price(), 1000 + 25 * 99);
    }

    #[test]
    fn tick_floor_and_ceil_clamp() {
        let l = Ladder::new(100, 10, 10); // 100..190
        assert_eq!(l.tick_floor(145), Some(4)); // 140
        assert_eq!(l.tick_floor(99), None);
        assert_eq!(l.tick_floor(10_000), Some(9)); // clamp 到頂
        assert_eq!(l.tick_ceil(145), Some(5)); // 150
        assert_eq!(l.tick_ceil(50), Some(0)); // clamp 到底
        assert_eq!(l.tick_ceil(10_000), None);
    }

    #[test]
    fn occupancy_tracks_push_and_unlink() {
        let mut a = OrderArena::with_capacity(8);
        let mut l = ladder();
        assert_eq!(l.lowest_tick(), None);

        let x = a.alloc(slot(5)).unwrap().slot();
        let y = a.alloc(slot(7)).unwrap().slot();
        l.push_back(&mut a, 10, x);
        l.push_back(&mut a, 200, y);

        assert_eq!(l.lowest_tick(), Some(10));
        assert_eq!(l.highest_tick(), Some(200));
        assert_eq!(l.live_levels(), 2);
        assert_eq!(l.level(10).volume, 5);

        l.unlink(&mut a, 10, x);
        assert_eq!(l.lowest_tick(), Some(200));
        assert_eq!(l.live_levels(), 1);

        l.unlink(&mut a, 200, y);
        assert_eq!(l.highest_tick(), None);
        assert_eq!(l.live_levels(), 0);
    }

    #[test]
    fn level_stays_occupied_until_last_order_leaves() {
        let mut a = OrderArena::with_capacity(8);
        let mut l = ladder();
        let x = a.alloc(slot(1)).unwrap().slot();
        let y = a.alloc(slot(2)).unwrap().slot();
        l.push_back(&mut a, 42, x);
        l.push_back(&mut a, 42, y);
        assert_eq!(l.live_levels(), 1);
        l.unlink(&mut a, 42, x);
        assert_eq!(l.lowest_tick(), Some(42)); // 仲有 y
        l.unlink(&mut a, 42, y);
        assert_eq!(l.lowest_tick(), None);
    }

    #[test]
    fn volume_between_sums_only_occupied_levels() {
        let mut a = OrderArena::with_capacity(16);
        let mut l = ladder();
        for (tick, qty) in [(5u32, 10u64), (7, 20), (9, 30), (100, 40)] {
            let s = a.alloc(slot(qty)).unwrap().slot();
            l.push_back(&mut a, tick, s);
        }
        assert_eq!(l.volume_between(0, 9), 60);
        assert_eq!(l.volume_between(6, 100), 90);
        assert_eq!(l.volume_between(101, 255), 0);
        assert_eq!(l.volume_between(9, 5), 0);
    }
}

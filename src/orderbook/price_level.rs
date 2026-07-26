//! Price level 淨係 hold **index**，唔 hold 任何 owned node，
//! 亦唔再 hold `price` —— 價格由 ladder 嘅 tick 推導出嚟。

use crate::orderbook::arena::OrderArena;
use crate::orderbook::types::{Price, Quantity, NIL};

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PriceLevel {
    pub volume: Quantity,
    /// FIFO 隊頭嘅 arena slot index（最早到 = 最高優先）
    pub head: u32,
    pub tail: u32,
    pub order_count: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LevelInfo {
    pub price: Price,
    pub volume: Quantity,
    pub order_count: u32,
}

impl PriceLevel {
    pub const EMPTY: PriceLevel = PriceLevel {
        volume: 0,
        head: NIL,
        tail: NIL,
        order_count: 0,
    };

    #[inline(always)]
    pub const fn is_empty(&self) -> bool {
        self.head == NIL
    }
}

/// 掛落隊尾 —— price-time priority 嘅 "time" 部分。
#[inline]
pub fn push_back(arena: &mut OrderArena, level: &mut PriceLevel, idx: u32) {
    let tail = level.tail;
    {
        let s = arena.at_mut(idx);
        s.prev = tail;
        s.next = NIL;
    }
    if tail == NIL {
        level.head = idx;
    } else {
        arena.at_mut(tail).next = idx;
    }
    level.tail = idx;
    level.volume += arena.at(idx).remaining_qty;
    level.order_count += 1;
}

/// O(1) 摘走鏈中任何一個 node。冇 `unsafe`、冇 dangling pointer。
#[inline]
pub fn unlink(arena: &mut OrderArena, level: &mut PriceLevel, idx: u32) {
    let (prev, next, qty) = {
        let s = arena.at(idx);
        (s.prev, s.next, s.remaining_qty)
    };
    if prev == NIL {
        level.head = next;
    } else {
        arena.at_mut(prev).next = next;
    }
    if next == NIL {
        level.tail = prev;
    } else {
        arena.at_mut(next).prev = prev;
    }
    {
        let s = arena.at_mut(idx);
        s.prev = NIL;
        s.next = NIL;
        s.level = NIL;
    }
    level.volume -= qty;
    level.order_count -= 1;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orderbook::arena::OrderSlot;
    use crate::orderbook::order::{OrderType, Side};
    use crate::orderbook::types::ClientOrderId;

    fn slot(qty: Quantity) -> OrderSlot {
        OrderSlot::incoming(
            ClientOrderId(0),
            OrderType::LimitOrder,
            Side::Buy,
            100,
            qty,
            0,
        )
    }

    fn chain(arena: &OrderArena, level: &PriceLevel) -> Vec<u32> {
        let mut out = Vec::new();
        let mut cur = level.head;
        while cur != NIL {
            out.push(cur);
            cur = arena.at(cur).next;
        }
        out
    }

    #[test]
    fn price_level_is_24_bytes() {
        assert_eq!(std::mem::size_of::<PriceLevel>(), 24);
    }

    #[test]
    fn fifo_order_is_preserved() {
        let mut a = OrderArena::with_capacity(8);
        let mut l = PriceLevel::EMPTY;
        let x = a.alloc(slot(1)).unwrap().slot();
        let y = a.alloc(slot(2)).unwrap().slot();
        let z = a.alloc(slot(3)).unwrap().slot();
        push_back(&mut a, &mut l, x);
        push_back(&mut a, &mut l, y);
        push_back(&mut a, &mut l, z);
        assert_eq!(chain(&a, &l), vec![x, y, z]);
        assert_eq!(l.volume, 6);
        assert_eq!(l.order_count, 3);
    }

    #[test]
    fn unlink_from_middle_keeps_chain_intact() {
        let mut a = OrderArena::with_capacity(8);
        let mut l = PriceLevel::EMPTY;
        let x = a.alloc(slot(1)).unwrap().slot();
        let y = a.alloc(slot(2)).unwrap().slot();
        let z = a.alloc(slot(3)).unwrap().slot();
        push_back(&mut a, &mut l, x);
        push_back(&mut a, &mut l, y);
        push_back(&mut a, &mut l, z);
        unlink(&mut a, &mut l, y);
        assert_eq!(chain(&a, &l), vec![x, z]);
        assert_eq!(l.volume, 4);
        assert_eq!(a.at(x).next, z);
        assert_eq!(a.at(z).prev, x);
    }

    #[test]
    fn unlink_head_and_tail() {
        let mut a = OrderArena::with_capacity(8);
        let mut l = PriceLevel::EMPTY;
        let x = a.alloc(slot(1)).unwrap().slot();
        let y = a.alloc(slot(2)).unwrap().slot();
        push_back(&mut a, &mut l, x);
        push_back(&mut a, &mut l, y);
        unlink(&mut a, &mut l, x);
        assert_eq!(l.head, y);
        assert_eq!(a.at(y).prev, NIL);
        unlink(&mut a, &mut l, y);
        assert!(l.is_empty());
        assert_eq!(l.tail, NIL);
        assert_eq!(l.volume, 0);
    }
}

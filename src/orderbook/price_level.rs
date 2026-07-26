//! Price level 而家淨係 hold **index**，唔再 hold 任何 owned node。
//!
//! 結果係 `PriceLevel` 變成 `Copy` 兼 24 bytes，可以直接住喺一個
//! `Vec<Option<PriceLevel>>`（step 7 會變成固定 ladder array）。
//! 原本嘅 `LinkedList<OrderNodeAdapter>` 同 `intrusive-collections`
//! 依賴一齊退休。

use crate::orderbook::arena::OrderArena;
use crate::orderbook::types::{Price, Quantity, NIL};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PriceLevel {
    pub price: Price,
    /// FIFO 隊頭嘅 arena slot index（最早到 = 最高優先）
    pub head: u32,
    /// FIFO 隊尾
    pub tail: u32,
    pub volume: Quantity,
    pub order_count: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LevelInfo {
    pub price: Price,
    pub volume: Quantity,
    pub order_count: u32,
}

impl PriceLevel {
    #[inline(always)]
    pub const fn new(price: Price) -> Self {
        PriceLevel {
            price,
            head: NIL,
            tail: NIL,
            volume: 0,
            order_count: 0,
        }
    }

    #[inline(always)]
    pub const fn is_empty(&self) -> bool {
        self.head == NIL
    }

    #[inline(always)]
    pub const fn info(&self) -> LevelInfo {
        LevelInfo {
            price: self.price,
            volume: self.volume,
            order_count: self.order_count,
        }
    }
}

/// 掛落隊尾。呢個係 price-time priority 嘅 "time" 部分。
///
/// 用 free function 而唔係 method，係因為 caller 要同時 borrow
/// `&mut self.arena` 同 `&mut self.levels[i]`——兩個係 disjoint field，
/// 拆開參數之後 borrow checker 就滿意。
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

/// O(1) 摘走鏈中**任何一個** node。
///
/// 呢個就係原本用 `NonNull<OrderNode>` + `cursor_mut_from_ptr` 想做嘅嘢，
/// 而家用 index 做：冇 `unsafe`、冇 dangling、cancel 一樣係 O(1)。
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
        OrderSlot::incoming(ClientOrderId(0), OrderType::LimitOrder, Side::Buy, 100, qty, 0)
    }

    fn ids(arena: &OrderArena, level: &PriceLevel) -> Vec<u32> {
        let mut out = Vec::new();
        let mut cur = level.head;
        while cur != NIL {
            out.push(cur);
            cur = arena.at(cur).next;
        }
        out
    }

    #[test]
    fn fifo_order_is_preserved() {
        let mut a = OrderArena::with_capacity(8);
        let mut l = PriceLevel::new(100);
        let x = a.alloc(slot(1)).unwrap().slot();
        let y = a.alloc(slot(2)).unwrap().slot();
        let z = a.alloc(slot(3)).unwrap().slot();
        push_back(&mut a, &mut l, x);
        push_back(&mut a, &mut l, y);
        push_back(&mut a, &mut l, z);

        assert_eq!(ids(&a, &l), vec![x, y, z]);
        assert_eq!(l.volume, 6);
        assert_eq!(l.order_count, 3);
    }

    #[test]
    fn unlink_from_middle_is_o1_and_keeps_chain_intact() {
        let mut a = OrderArena::with_capacity(8);
        let mut l = PriceLevel::new(100);
        let x = a.alloc(slot(1)).unwrap().slot();
        let y = a.alloc(slot(2)).unwrap().slot();
        let z = a.alloc(slot(3)).unwrap().slot();
        push_back(&mut a, &mut l, x);
        push_back(&mut a, &mut l, y);
        push_back(&mut a, &mut l, z);

        unlink(&mut a, &mut l, y);
        assert_eq!(ids(&a, &l), vec![x, z]);
        assert_eq!(l.volume, 4);
        assert_eq!(a.at(x).next, z);
        assert_eq!(a.at(z).prev, x);
    }

    #[test]
    fn unlink_head_and_tail() {
        let mut a = OrderArena::with_capacity(8);
        let mut l = PriceLevel::new(100);
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
        assert_eq!(l.order_count, 0);
    }
}

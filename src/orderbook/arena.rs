//! 固定容量嘅 order arena。
//!
//! 呢個 module 取代咗三樣嘢：
//!   * `Arc<Order>`          —— 每張單一次 heap alloc + atomic refcount
//!   * `Box<OrderNode>`      —— 每張單再一次 heap alloc
//!   * `NonNull<OrderNode>`  —— partial fill 之後會變 dangling 嘅 raw pointer
//!
//! 全部 slot 喺 `with_capacity` 一次過分配好，之後 `alloc`/`free` 只係
//! 郁一個 `u32` free-list head。滿咗係 `None`（backpressure），唔會 grow。

use crate::orderbook::order::{OrderType, Side, Status};
use crate::orderbook::types::{ClientOrderId, OrderId, Price, Quantity, NIL};

/// 一張單。刻意砌到啱啱 64 bytes = 一條 cache line。
///
/// 冇 `executed_qty` field —— 佢等於 `original_qty - remaining_qty`，
/// 原本兩個都存住反而係 state 唔一致嘅來源。
/// 冇 `timestamp` —— FIFO priority 用 `seq`（單調 counter），
/// wall clock 會被 NTP 拉倒退，唔可以用嚟排優先次序。
/// 冇 explicit padding field —— `repr(C)` 會自己補尾部 padding。
/// 冇 padding field 就冇「比較 padding bytes」呢個問題，
/// `PartialEq` 先至 derive 得心安理得。
///
/// `align(64)` 唔係為咗防 false sharing（engine 係單線程，冇並發寫者，
/// 所以唔需要 `crossbeam_utils::CachePadded` —— 佢喺 x86_64/aarch64 仲要
/// 撐到 128 bytes，只會令密度差一倍）。佢係為咗保證**冇 slot 跨兩條 line**：
/// `size_of == 64` 本身唔保證每個元素落喺 64-byte 邊界，要 align 先得。
/// 有咗 align(64)，`Vec` 嘅 backing allocation 亦會跟住 64 對齊，
/// 成個 arena 就完美鋪砌。
#[repr(C, align(64))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OrderSlot {
    /// 每次 free 遞增。舊 `OrderId` 攞返嚟 resolve 會對唔上 -> 安全 reject。
    pub generation: u32,
    /// 價位內 FIFO 嘅下一個；slot 空閒時做 free-list link。
    pub next: u32,
    /// 價位內 FIFO 嘅上一個。
    pub prev: u32,
    /// 所屬 price level index；未掛單時係 `NIL`。
    pub level: u32,

    pub price: Price,
    pub original_qty: Quantity,
    pub remaining_qty: Quantity,
    pub client_order_id: ClientOrderId,
    /// 單調遞增嘅到達序號，用嚟做 price-time priority 嘅 time 部分。
    pub seq: u64,

    pub side: Side,
    pub order_type: OrderType,
    pub status: Status,
    pub in_use: bool,
}

impl OrderSlot {
    pub const EMPTY: OrderSlot = OrderSlot {
        generation: 1,
        next: NIL,
        prev: NIL,
        level: NIL,
        price: 0,
        original_qty: 0,
        remaining_qty: 0,
        client_order_id: ClientOrderId(0),
        seq: 0,
        side: Side::Buy,
        order_type: OrderType::LimitOrder,
        status: Status::New,
        in_use: false,
    };

    /// 由一張新單起一個 slot。
    ///
    /// 用 `..OrderSlot::EMPTY` 嘅 struct update syntax 會 E0451。
    pub const fn incoming(
        client_order_id: ClientOrderId,
        order_type: OrderType,
        side: Side,
        price: Price,
        quantity: Quantity,
        seq: u64,
    ) -> Self {
        OrderSlot {
            generation: 1,
            next: NIL,
            prev: NIL,
            level: NIL,
            price,
            original_qty: quantity,
            remaining_qty: quantity,
            client_order_id,
            seq,
            side,
            order_type,
            status: Status::New,
            in_use: false,
        }
    }

    #[inline(always)]
    pub const fn executed_qty(&self) -> Quantity {
        self.original_qty - self.remaining_qty
    }

    #[inline(always)]
    pub const fn is_resting(&self) -> bool {
        self.level != NIL
    }
}

pub struct OrderArena {
    slots: Box<[OrderSlot]>,
    free_head: u32,
    live: u32,
}

impl OrderArena {
    /// 一次過分配 `capacity` 個 slot 並串成 free list。
    /// 呢度係整個 engine 唯一會為 order 做 allocation 嘅地方。
    pub fn with_capacity(capacity: usize) -> Self {
        assert!(capacity > 0, "arena capacity must be > 0");
        assert!(
            capacity < NIL as usize,
            "arena capacity must be < u32::MAX (NIL sentinel)"
        );

        let mut slots = vec![OrderSlot::EMPTY; capacity];
        for (i, s) in slots.iter_mut().enumerate() {
            s.next = if i + 1 < capacity { (i + 1) as u32 } else { NIL };
        }

        OrderArena {
            slots: slots.into_boxed_slice(),
            free_head: 0,
            live: 0,
        }
    }

    #[inline(always)]
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    #[inline(always)]
    pub fn live(&self) -> u32 {
        self.live
    }

    #[inline(always)]
    pub fn is_full(&self) -> bool {
        self.free_head == NIL
    }

    /// 攞一個 slot。滿 -> `None`。**永遠唔會 allocate。**
    pub fn alloc(&mut self, init: OrderSlot) -> Option<OrderId> {
        let idx = self.free_head;
        if idx == NIL {
            return None;
        }
        let s = &mut self.slots[idx as usize];
        self.free_head = s.next;

        let generation = s.generation; // generation 唔可以被 init 覆蓋
        *s = init;
        s.generation = generation;
        s.in_use = true;
        s.next = NIL;
        s.prev = NIL;
        s.level = NIL;

        self.live += 1;
        Some(OrderId::new(idx, generation))
    }

    /// 還一個 slot。generation 遞增，所有仲揸住舊 `OrderId` 嘅人自動失效。
    ///
    /// 注意 generation 用 `wrapping_add`：理論上 2^32 次重用之後會 ABA。
    /// 以 1M orders/sec 計，同一個 slot 要重用 2^32 次大約要幾十年。
    pub fn free(&mut self, id: OrderId) -> bool {
        let Some(s) = self.slots.get_mut(id.slot() as usize) else {
            return false;
        };
        if !s.in_use || s.generation != id.generation() {
            return false;
        }
        s.in_use = false;
        s.generation = s.generation.wrapping_add(1);
        s.level = NIL;
        s.prev = NIL;
        s.next = self.free_head;
        self.free_head = id.slot();
        self.live -= 1;
        true
    }

    /// 由 `OrderId` 攞返個 slot。stale ID -> `None`（唔係 UB）。
    #[inline(always)]
    pub fn get(&self, id: OrderId) -> Option<&OrderSlot> {
        let s = self.slots.get(id.slot() as usize)?;
        if s.in_use && s.generation == id.generation() {
            Some(s)
        } else {
            None
        }
    }

    #[inline(always)]
    pub fn get_mut(&mut self, id: OrderId) -> Option<&mut OrderSlot> {
        let s = self.slots.get_mut(id.slot() as usize)?;
        if s.in_use && s.generation == id.generation() {
            Some(s)
        } else {
            None
        }
    }

    /// 內部用：直接用 index 攞 slot（行 FIFO 鏈嘅時候）。
    #[inline(always)]
    pub fn at(&self, idx: u32) -> &OrderSlot {
        &self.slots[idx as usize]
    }

    #[inline(always)]
    pub fn at_mut(&mut self, idx: u32) -> &mut OrderSlot {
        &mut self.slots[idx as usize]
    }

    /// 俾 `mem::warm` 用嚟 pre-fault / mlock 整塊 arena。
    #[inline]
    pub fn slots_mut(&mut self) -> &mut [OrderSlot] {
        &mut self.slots
    }

    #[inline(always)]
    pub fn id_at(&self, idx: u32) -> OrderId {
        OrderId::new(idx, self.slots[idx as usize].generation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> OrderSlot {
        OrderSlot {
            price: 100,
            original_qty: 10,
            remaining_qty: 10,
            ..OrderSlot::EMPTY
        }
    }

    #[test]
    fn order_slot_is_exactly_one_cache_line() {
        assert_eq!(std::mem::size_of::<OrderSlot>(), 64);
        // size 啱唔夠 —— 要 align 先保證冇元素跨 line
        assert_eq!(std::mem::align_of::<OrderSlot>(), 64);
    }

    #[test]
    fn every_slot_starts_on_a_cache_line_boundary() {
        let a = OrderArena::with_capacity(8);
        for i in 0..8u32 {
            let addr = a.at(i) as *const OrderSlot as usize;
            assert_eq!(addr % 64, 0, "slot {i} straddles a cache line");
        }
    }

    #[test]
    fn alloc_and_resolve() {
        let mut a = OrderArena::with_capacity(4);
        let id = a.alloc(sample()).unwrap();
        assert_eq!(a.live(), 1);
        assert_eq!(a.get(id).unwrap().remaining_qty, 10);
    }

    #[test]
    fn arena_full_returns_none_and_never_grows() {
        let mut a = OrderArena::with_capacity(2);
        assert!(a.alloc(sample()).is_some());
        assert!(a.alloc(sample()).is_some());
        assert!(a.alloc(sample()).is_none());
        assert_eq!(a.capacity(), 2);
    }

    /// 呢個 test 就係原本 use-after-free 嘅替代品：
    /// 舊 handle 唔會 deref 到已釋放嘅記憶體，只會 resolve 唔到。
    #[test]
    fn stale_id_does_not_resolve() {
        let mut a = OrderArena::with_capacity(2);
        let old = a.alloc(sample()).unwrap();
        assert!(a.free(old));
        assert!(a.get(old).is_none());
        assert!(!a.free(old)); // double free 亦都係安全 no-op

        // 同一個 slot 被重用，但 generation 唔同 -> 兩個 ID 唔會撞
        let new = a.alloc(sample()).unwrap();
        assert_eq!(new.slot(), old.slot());
        assert_ne!(new.generation(), old.generation());
        assert!(a.get(old).is_none());
        assert!(a.get(new).is_some());
    }

    #[test]
    fn slots_are_recycled_lifo() {
        let mut a = OrderArena::with_capacity(3);
        let a1 = a.alloc(sample()).unwrap();
        let a2 = a.alloc(sample()).unwrap();
        a.free(a1);
        a.free(a2);
        assert_eq!(a.live(), 0);
        // 兩個都還返，仲可以再攞三個
        assert!(a.alloc(sample()).is_some());
        assert!(a.alloc(sample()).is_some());
        assert!(a.alloc(sample()).is_some());
        assert!(a.alloc(sample()).is_none());
    }

    #[test]
    fn executed_qty_is_derived() {
        let mut s = sample();
        s.remaining_qty = 4;
        assert_eq!(s.executed_qty(), 6);
    }
}

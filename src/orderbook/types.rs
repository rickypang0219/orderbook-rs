use std::fmt;

pub type Price = i64;
pub type Quantity = u64;

/// Sentinel for "no index". Arena capacity must stay below this.
pub const NIL: u32 = u32::MAX;

/// Engine 分配嘅 order ID。
///
/// 高 32 bit = generation，低 32 bit = arena slot index。
///
/// 咁樣做嘅意義：
/// * lookup 由「hash -> bucket probe -> pointer chase」變成
///   「一次 bounds check + 一次 u32 compare + 一次 array index」；
///   `HashMap<OrderId, OrderEntry>` 連同佢嘅 rehash spike 完全消失。
/// * generation 令舊 handle 自動失效。一張單 fill 咗之後，
///   佢個 ID 再攞返嚟 resolve 會係 `None` 而唔係 dangling pointer。
///
/// 注意呢個係 **exchange-assigned** ID（FIX tag 37），
/// 唔係 client 提供嘅 `ClientOrderId`（FIX tag 11）。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct OrderId(u64);

impl OrderId {
    pub const INVALID: OrderId = OrderId(u64::MAX);

    #[inline(always)]
    pub const fn new(slot: u32, generation: u32) -> Self {
        OrderId(((generation as u64) << 32) | (slot as u64))
    }

    #[inline(always)]
    pub const fn slot(self) -> u32 {
        self.0 as u32
    }

    #[inline(always)]
    pub const fn generation(self) -> u32 {
        (self.0 >> 32) as u32
    }

    #[inline(always)]
    pub const fn raw(self) -> u64 {
        self.0
    }
}

impl fmt::Display for OrderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}g{}", self.slot(), self.generation())
    }
}

/// 同 OrderId 唔同命名空間（FIX tag 17 ExecID）。
/// 用單調 counter，唔用 UUID —— 慳走每筆成交一次 CSPRNG。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct TradeId(pub u64);

impl fmt::Display for TradeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "T{}", self.0)
    }
}

/// Client-provided metadata (FIX tag 11); uniqueness belongs to the caller.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct ClientOrderId(pub u64);

impl fmt::Display for ClientOrderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "C{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_id_roundtrip() {
        let id = OrderId::new(12_345, 7);
        assert_eq!(id.slot(), 12_345);
        assert_eq!(id.generation(), 7);
    }

    #[test]
    fn order_id_slot_and_generation_are_independent() {
        let a = OrderId::new(1, 2);
        let b = OrderId::new(2, 1);
        assert_ne!(a, b);
    }
}

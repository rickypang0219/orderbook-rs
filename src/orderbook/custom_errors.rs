use crate::orderbook::types::Quantity;

/// Copy 型 error：冇 String，冇 format!，冇 heap allocation。
/// 人類可讀嘅訊息交由 Display 喺 log/journal 邊界先 format。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum QuantityError {
    #[error("fill quantity {requested} exceeds remaining {remaining}")]
    Overfill {
        remaining: Quantity,
        requested: Quantity,
    },
}

//! 階層式 bitmap。
//!
//! 每一層嘅一個 bit 概括下一層一整個 `u64` word：
//!
//! ```text
//! L2  [ ................................................ ]   1 word
//! L1  [ ........ ][ ........ ] ...                          N/4096 words
//! L0  [ tick 0..63 ][ tick 64..127 ] ...                    N/64  words
//! ```
//!
//! 咁 "邊個 tick 有單" 就變成一個可以喺 O(depth) 步、每步一條
//! `trailing_zeros` / `leading_zeros` 指令答到嘅問題。N = 65536 嘅時候
//! depth = 3，即係最壞情況三條指令搵到 best bid/ask —— 對比
//! `BTreeMap::iter().next()` 每次都要行返 tree 最左路徑。
//!
//! 所有 word 喺 `with_capacity` 一次過分配，之後零 allocation。

pub struct HierBitset {
    /// `levels[0]` 係最細粒度；最後一層永遠啱啱一個 word。
    levels: Vec<Box<[u64]>>,
    capacity: usize,
}

impl HierBitset {
    pub fn with_capacity(capacity: usize) -> Self {
        assert!(capacity > 0, "bitset capacity must be > 0");
        let mut levels: Vec<Box<[u64]>> = Vec::new();
        let mut n = capacity;
        loop {
            let words = n.div_ceil(64);
            levels.push(vec![0u64; words].into_boxed_slice());
            if words == 1 {
                break;
            }
            n = words;
        }
        HierBitset { levels, capacity }
    }

    #[inline(always)]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    #[inline(always)]
    pub fn depth(&self) -> usize {
        self.levels.len()
    }

    #[inline(always)]
    pub fn contains(&self, i: usize) -> bool {
        i < self.capacity && (self.levels[0][i >> 6] >> (i & 63)) & 1 == 1
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.levels[self.levels.len() - 1][0] == 0
    }

    /// 設一個 bit，並向上傳播。每層一次 OR，無分支。
    #[inline]
    pub fn insert(&mut self, i: usize) {
        debug_assert!(i < self.capacity);
        let mut idx = i;
        for level in self.levels.iter_mut() {
            let w = idx >> 6;
            level[w] |= 1u64 << (idx & 63);
            idx = w;
        }
    }

    /// 清一個 bit。只有當成個 word 變 0 先需要向上傳播，
    /// 所以通常一層就收工。
    #[inline]
    pub fn remove(&mut self, i: usize) {
        debug_assert!(i < self.capacity);
        let mut idx = i;
        for level in self.levels.iter_mut() {
            let w = idx >> 6;
            level[w] &= !(1u64 << (idx & 63));
            if level[w] != 0 {
                return;
            }
            idx = w;
        }
    }

    /// 最細嘅 >= `i` 嘅已設 bit。
    pub fn next_at_or_above(&self, i: usize) -> Option<usize> {
        if i >= self.capacity {
            return None;
        }
        let mut level = 0usize;
        let mut idx = i;
        loop {
            let w = idx >> 6;
            if w >= self.levels[level].len() {
                return None;
            }
            let bits = self.levels[level][w] & (u64::MAX << (idx & 63));
            if bits != 0 {
                let mut found = (w << 6) | bits.trailing_zeros() as usize;
                // 由概括層一路落返 L0
                for l in (0..level).rev() {
                    let word = self.levels[l][found];
                    debug_assert!(word != 0, "summary bit set but word is zero");
                    found = (found << 6) | word.trailing_zeros() as usize;
                }
                return (found < self.capacity).then_some(found);
            }
            level += 1;
            if level >= self.levels.len() {
                return None;
            }
            idx = w + 1;
        }
    }

    /// 最大嘅 <= `i` 嘅已設 bit。
    pub fn next_at_or_below(&self, i: usize) -> Option<usize> {
        if self.capacity == 0 {
            return None;
        }
        let start = i.min(self.capacity - 1);
        let mut level = 0usize;
        let mut idx: i64 = start as i64;
        loop {
            if idx < 0 {
                return None;
            }
            let u = idx as usize;
            let w = u >> 6;
            if w >= self.levels[level].len() {
                return None;
            }
            let b = u & 63;
            let mask = if b == 63 {
                u64::MAX
            } else {
                (1u64 << (b + 1)) - 1
            };
            let bits = self.levels[level][w] & mask;
            if bits != 0 {
                let mut found = (w << 6) | (63 - bits.leading_zeros() as usize);
                for l in (0..level).rev() {
                    let word = self.levels[l][found];
                    debug_assert!(word != 0, "summary bit set but word is zero");
                    found = (found << 6) | (63 - word.leading_zeros() as usize);
                }
                return Some(found);
            }
            level += 1;
            if level >= self.levels.len() {
                return None;
            }
            idx = w as i64 - 1;
        }
    }

    #[inline]
    pub fn lowest(&self) -> Option<usize> {
        self.next_at_or_above(0)
    }

    #[inline]
    pub fn highest(&self) -> Option<usize> {
        self.next_at_or_below(self.capacity - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 對照組：暴力 Vec<bool>，用嚟驗證階層版本
    struct Naive(Vec<bool>);
    impl Naive {
        fn next_above(&self, i: usize) -> Option<usize> {
            (i..self.0.len()).find(|&k| self.0[k])
        }
        fn next_below(&self, i: usize) -> Option<usize> {
            (0..=i.min(self.0.len() - 1)).rev().find(|&k| self.0[k])
        }
    }

    #[test]
    fn depth_grows_logarithmically() {
        assert_eq!(HierBitset::with_capacity(64).depth(), 1);
        assert_eq!(HierBitset::with_capacity(65).depth(), 2);
        assert_eq!(HierBitset::with_capacity(4096).depth(), 2);
        assert_eq!(HierBitset::with_capacity(4097).depth(), 3);
        assert_eq!(HierBitset::with_capacity(65536).depth(), 3);
    }

    #[test]
    fn insert_remove_contains() {
        let mut b = HierBitset::with_capacity(1000);
        assert!(b.is_empty());
        b.insert(0);
        b.insert(999);
        b.insert(500);
        assert!(b.contains(0) && b.contains(500) && b.contains(999));
        assert!(!b.contains(1));
        assert_eq!(b.lowest(), Some(0));
        assert_eq!(b.highest(), Some(999));

        b.remove(0);
        b.remove(999);
        assert_eq!(b.lowest(), Some(500));
        assert_eq!(b.highest(), Some(500));
        b.remove(500);
        assert!(b.is_empty());
        assert_eq!(b.lowest(), None);
        assert_eq!(b.highest(), None);
    }

    #[test]
    fn word_boundaries() {
        let mut b = HierBitset::with_capacity(300);
        for i in [63, 64, 127, 128, 191, 192] {
            b.insert(i);
        }
        assert_eq!(b.next_at_or_above(0), Some(63));
        assert_eq!(b.next_at_or_above(64), Some(64));
        assert_eq!(b.next_at_or_above(65), Some(127));
        assert_eq!(b.next_at_or_below(126), Some(64));
        assert_eq!(b.next_at_or_below(63), Some(63));
        assert_eq!(b.next_at_or_below(62), None);
    }

    #[test]
    fn matches_naive_on_pseudorandom_pattern() {
        const N: usize = 5000;
        let mut b = HierBitset::with_capacity(N);
        let mut naive = Naive(vec![false; N]);

        // 一個確定性偽隨機序列，唔使引入 rand 依賴
        let mut state: u64 = 0x243F_6A88_85A3_08D3;
        for _ in 0..N {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let i = (state >> 33) as usize % N;
            if naive.0[i] {
                b.remove(i);
                naive.0[i] = false;
            } else {
                b.insert(i);
                naive.0[i] = true;
            }
        }

        for i in 0..N {
            assert_eq!(b.contains(i), naive.0[i], "contains mismatch at {i}");
            assert_eq!(b.next_at_or_above(i), naive.next_above(i), "above at {i}");
            assert_eq!(b.next_at_or_below(i), naive.next_below(i), "below at {i}");
        }
    }

    #[test]
    fn out_of_range_queries_are_safe() {
        let b = HierBitset::with_capacity(100);
        assert_eq!(b.next_at_or_above(100), None);
        assert_eq!(b.next_at_or_above(usize::MAX), None);
        assert_eq!(b.next_at_or_below(usize::MAX), None);
    }
}

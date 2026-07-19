# orderbook-rs

A price-time-priority matching engine written in Rust. The project focuses on the mechanics that matter in an exchange order book: deterministic matching, price-level indexing, FIFO execution, constant-time cancellation within a level, and explicit management of a small unsafe boundary.

## Design

```rust
pub struct OrderBook {
    bids: BTreeMap<Reverse<Price>, PriceLevelRef>,
    asks: BTreeMap<Price, PriceLevelRef>,
    orders: HashMap<OrderId, OrderEntry>,
    price_levels: Vec<Option<PriceLevel>>,
}
```

- `BTreeMap` keeps price levels ordered and exposes the best bid/ask in O(log P), where `P` is active price levels.
- Each price level is an intrusive FIFO linked list, preserving time priority.
- `orders` maps an order ID to an owning node handle, enabling O(1) average lookup and unlink for cancellation.
- Empty price-level slots are recycled to reduce allocation churn.

The cancellation fast path is O(1) average. Cancelling the final order at a price also removes its `BTreeMap` entry, which is O(log P), where `P` is the number of price levels. This distinction is intentional: describing every cancel as unconditionally O(1) would hide the price-level cleanup cost.

## Order semantics

| Type | Behaviour |
|---|---|
| Limit / GTC | Match up to the limit price, then rest any remainder |
| Market | Match available liquidity at any price; never rests |
| IOC | Match immediately up to the limit price; cancel any remainder |
| FOK | Execute the full quantity immediately or make no book change |

Trades execute at the resting order's price. Orders at the same price execute FIFO.

## Correctness and unsafe-code discipline

The order index stores an owning `Rc<OrderNode>` handle while the intrusive list owns another clone. A raw pointer is created only transiently, through `Rc::as_ptr`, to construct the cancellation cursor. Matching uses the safe `front_mut` cursor. The safety contract is:

1. the indexed handle and list entry are created together;
2. the owning handle keeps the node alive for the entire unsafe cursor operation;
3. a partial fill that replaces a node also replaces the indexed handle;
4. cancellation removes the list node and its index entry in the same operation.

`OrderBook::validate_invariants` checks the pointer index, FIFO nodes, price maps, order counts, per-level volume, total quantity accounting, free-list integrity, and the uncrossed-book condition. Proptest applies random sequences of limit, market, IOC, FOK, and cancel operations and validates these invariants after every operation. A focused Miri test exercises the partial-fill/node-replacement/cancel lifecycle.

```bash
cargo test --all-targets
MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test --lib partial_fill_updates_pointer_before_cancel
```

CI runs formatting, Clippy with warnings denied, the full test suite, and the focused Miri check.

## Benchmark methodology

Benchmarks use Criterion in release mode:

```bash
cargo bench --bench orderbook
```

Each sample processes a batch of 10,000 operations. `iter_batched` keeps book construction and input generation outside the timed section. Orders use deterministic, pre-generated UUIDs, so RNG, UUID generation, and input timestamps are not charged to add/cancel latency. The matching benchmark does include trade allocation, trade UUID generation, timestamps, filled-order removal, and index maintenance because those are part of the matching path.

Measured on a 14-inch MacBook Pro with M1 Max (20 Criterion samples, five-second measurement window):

| Operation | Estimated throughput | 95% interval |
|---|---:|---:|
| Add resting orders, default capacity | 5.89 M orders/s | 5.88–5.91 M/s |
| Add resting orders, preallocated | 7.33 M orders/s | 7.32–7.35 M/s |
| Cancel orders | 7.30 M cancels/s | 7.29–7.31 M/s |
| Match one resting order per incoming order | 0.81 M matches/s | 0.80–0.82 M/s |

The previous ~150K add/s result was not representative. Its timed loop generated random inputs, UUIDs, timestamps, and heap allocations. More importantly, every incoming order allocated a trade vector with capacity equal to the entire resting-order count, turning a normally empty result into O(n) allocation work. The current implementation starts with an empty trade vector, and the Criterion setup isolates the operation under test.

Criterion HTML reports are written under `target/criterion/report/index.html`.

## Complexity

Let `P` be active price levels and `F` the number of resting orders filled.

| Operation | Complexity |
|---|---|
| Add to an existing level | O(1) average index work + O(1) FIFO append |
| Add a new price level | O(log P) |
| Cancel | O(1) average; O(log P) when removing an empty level |
| Best bid / ask | O(log P) |
| Match | O(F log P) in the current implementation |

## CV-ready summary

> Built a Rust price-time-priority matching engine supporting Limit/GTC, Market, IOC, and FOK orders. Designed ordered price-level indexing with intrusive FIFO queues and O(1)-average cancellation; managed raw-pointer invariants with property-based testing, structural validation, Miri, and CI. Reworked Criterion benchmarks to isolate setup from measurement, demonstrating 5.9–7.3M resting adds/s and 7.3M cancels/s on M1 Max.

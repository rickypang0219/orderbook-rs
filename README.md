# Rust order book

Single-threaded, price-time priority matching with a fixed price band and capacity.
The matching engine uses safe Rust and allocates only during construction.

## Design

- A fixed arena stores 64-byte orders. IDs contain a slot index and generation,
  so lookup needs no hash table and stale IDs are rejected.
- Each price level holds an index-based doubly linked FIFO. Cancellation unlinks
  an order directly; partial fills update it in place.
- Prices map to a dense tick ladder. A hierarchical bitmap locates occupied levels.
- Callers own and reuse a fixed-capacity `TradeBuf`.

Let `D = O(log₆₄ T)` for `T` price ticks, `k` be the number of fills, and `L`
be the number of visited price levels. The default grid has three bitmap layers.

| Operation | Cost |
|---|---|
| Lookup by engine ID | O(1) |
| FIFO append / unlink | O(1); O(D) when level occupancy changes |
| Best bid / ask | O(D) |
| Same-price quantity reduction | O(1) |
| Matching | O(k + LD), draining each FIFO before the next bitmap search |
| FOK liquidity check | O(LD), stops when sufficient quantity is found |

`submit` also validates price, capacity and output space. When a constant-time
upper bound cannot prove enough trade-buffer space, it scans prospective fills
before making any changes. That scan can add O(k + LD) work. These bounds assume
engine-assigned IDs; `ClientOrderId` is metadata, not a uniqueness index.

## Orders and amendments

Limit/GTC remainders rest on the book. Market/IOC remainders are canceled. FOK
executes completely or is killed; a killed FOK returns an already-inactive ID.

```rust
use orderbook::{ClientOrderId, NewOrder, OrderBook, Side, TradeBuf};

let mut book = OrderBook::new();
let mut trades = TradeBuf::with_capacity(1024);
let id = book.submit(
    &NewOrder::limit(ClientOrderId(1), Side::Buy, 100, 10),
    &mut trades,
    0,
).unwrap();
book.amend(id, 100, 5, &mut trades, 0).unwrap();
book.cancel(id).unwrap();
```

`amend(id, price, remaining, out, timestamp)` keeps the engine ID:

- Same-price reductions preserve FIFO priority; increases and reprices lose it.
- Reprices can match immediately. Zero remaining cancels, ignoring price.
- Already executed quantity stays unchanged. `original_qty` becomes the amended
  total (`executed + remaining`), rather than the first submitted quantity.

On any returned error, `submit` and `amend` leave the book and trade buffer
unchanged. A full output buffer never causes unreported fills. Existing trades
remain in the buffer; callers clear it after consuming them.

Price-level volumes and amended totals reject overflow. Sequence/trade IDs never
wrap. Exhausted slot generations permanently retire their slots, reducing usable
capacity. Submissions need a free arena slot even when they would immediately fill.

## Verification and benchmarks

```sh
cargo test --all-targets
cargo test --release --all-targets
cargo clippy --all-targets -- -D warnings
cargo run --release --bin benchmark
cargo run --release --bin latency
```

Tests cover output exhaustion, amend priority, stale handles, overflow, allocation
counts, and 5,000 deterministic mixed commands against a simple reference book.

The throughput benchmark generates random inputs before timing. The latency
benchmark installs the counting allocator, separates stale-ID rejects from
successful operations, and measures reductions, increases, reprices and price-level
creation/removal separately. Measurements include timer overhead and are closed-loop;
they do not measure queueing delay under load. Compare identical workloads/builds,
not the old README's historical throughput figures.

## Unsafe boundary

No unchecked indexing or raw pointers are used for matching. Existing unsafe code
is confined to the counting allocator and optional OS memory warm-up. Warm-up uses
volatile **typed `Copy` values** to prevent page touches being optimized away;
it does not read potentially uninitialized struct padding as `u8`.
See [Rust's volatile safety requirements](https://doc.rust-lang.org/std/ptr/fn.read_volatile.html#safety)
and [Rustonomicon: uninitialized memory](https://doc.rust-lang.org/nomicon/uninitialized.html).
Memory locking / huge-page advice is best-effort; inspect the returned `MemReport`.

Order-state bitmaps, cached best prices and dirty-level feeds are deferred until
profiling or an actual consumer justifies their additional state. Packing flags
alone would not shrink the currently 64-byte-aligned order slots.

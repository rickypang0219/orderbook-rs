# Orderbook implementation in Rust
A high performance orderbook implementation in Rust. This project is inspired by [orderbook-rust fjmuria](https://github.com/fjmurcia/orderbook-rust) and [CodingJesus C++ orderbook](https://github.com/Tzadiko/Orderbook).


# Difference between Orderbook-rust
 The main difference between [orderbook-rust by fjmurcia](https://github.com/fjmurcia/orderbook-rust) is that in our orderbook, we use `instrusive linked list` to store the orders inside price level instead of Vector End Queue. Using VecDeque cannot achive O(1) removal when canceling order. The true magic is to use unsafe pointer and instrusive linked list to remove orderNode, this is a true O(1) operation.


# Orderbook Design
```rust
pub struct OrderBook {
    bids: BTreeMap<Reverse<Price>, PriceLevel>,
    asks: BTreeMap<Price, PriceLevel>,
    orders: HashMap<OrderId, OrderEntry>,
}
```
The `Reverse` indicates that the BTreeMap searches price from largest to smallest number. This helps us to get the best bid price by `iter().next()` method easily, which is different from `fjmurcia` implementation design and similar to `CodingJesus` implementation in C++ with
```c
std::map<Price, OrderPointers, std::greater<Price>> bids_;

```


## Supported Order Types
| Type | Description |
|------|-------------|
| **Limit** | Order with limit price, sit in the book and wait to fill |
| **Market** | Order executed at any prices |
| **IOC** (Immediate or Cancel) | Executed either partially or cancelled, immediately (Not Implemented) |
| **FOK** (Fill or Kill) | Executed either entirely or rejected, immediately |
| **GTC** (Good Till Cancel) | Valid until cancelled |

# Performance
You can run the benchmark in `release` mode by

```
cargo run --release --bin benchmark
```
and you can see AddOrders/CancelOrders/MatchOrders latency, throughput, and trade rate information. The below benchmark is ran in Macbook Pro 14' with M1 Max 32GB RAM model.

| Operation | Complexity | Measured Throughput |
|-----------|------------|-------------------|
| Add Order | O(log n) | ~150K ops/sec |
| Cancel Order | O(1) | ~5M ops/sec |
| Matching | O(k log n) | ~150K matches/sec |


# Future Improvements
- WebSocket Data Feed with Binance Futures
- Do not store PriceLevel inside BTreeMap

```rust
#[derive(Clone, Copy, Debug)]
struct PriceLevelRef {
    index: usize,  // Index into the price_levels vector
    price: Price,  // Copy of the price for quick comparison
}

// Without PriceLevelRef - PROBLEMATIC:
struct BadDesign {
    bids: BTreeMap<Reverse<Price>, PriceLevel>,  // Large objects in tree!
    // Every tree rebalance moves entire PriceLevel objects!
}

// With PriceLevelRef - OPTIMAL:
struct GoodDesign {
    bids: BTreeMap<Reverse<Price>, PriceLevelRef>,  // Tiny references in tree
    price_levels: Vec<PriceLevel>,  // Stable storage location
}
```

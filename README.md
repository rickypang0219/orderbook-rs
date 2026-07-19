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
The `Reverse` indicates that the BTreeMap searches price from largest to smallest number. This helps us to get the best bid price by `iter().next()` method easily, which is different from `fjmurcia` implementation design but similar/closer to [CodingJesus bids order map implementation](https://github.com/Tzadiko/Orderbook/blob/dd136dd219ead95796f0e396e9e1395542bf673f/Orderbook.h#L39C5-L39C63) with
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
- Replace Linked List by VecDeque (similar to ring buffer approach)
```rust
orders: LinkedList<OrderNode>

new_orders: VecDeque<Option<OrderNode>> // may be fixed size array

// O(1) removal -> set the VecDeque[index] to None, and release the index/indices for rewrite
// better cache friendiness since continguous memory, no allocation if we use fixed size array
// cancel order does not mean we have to really remove the data, setting to None and update the price level correctly then done

```

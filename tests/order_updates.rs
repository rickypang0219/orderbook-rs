use orderbook::{
    BookConfig, ClientOrderId, NewOrder, OrderBook, OrderBookError as Error, OrderId, OrderType,
    Side, Trade, TradeBuf,
};

fn book() -> OrderBook {
    OrderBook::with_config(BookConfig {
        max_orders: 32,
        base_price: 0,
        tick_size: 1,
        num_ticks: 256,
    })
}

fn add(book: &mut OrderBook, out: &mut TradeBuf, side: Side, price: i64, qty: u64) -> OrderId {
    book.submit(&NewOrder::limit(ClientOrderId(1), side, price, qty), out, 0)
        .unwrap()
}

#[test]
fn insufficient_output_rejects_entire_order_for_both_sides_and_all_types() {
    for side in [Side::Buy, Side::Sell] {
        for kind in [
            OrderType::LimitOrder,
            OrderType::MarketOrder,
            OrderType::ImmediateOrCancel,
            OrderType::FillOrKill,
            OrderType::GoodTillCancel,
        ] {
            // Exercise zero space, prefilled output, and failure after one possible fill.
            for (capacity, prefill) in [(0, false), (1, false), (2, true)] {
                let mut book = book();
                let mut out = TradeBuf::with_capacity(capacity);
                let a = add(&mut book, &mut out, side.opposite(), 100, 2);
                let b = add(&mut book, &mut out, side.opposite(), 100, 2);
                if prefill {
                    assert!(out.push(Trade::EMPTY));
                }
                let before = (*book.get(a).unwrap(), *book.get(b).unwrap());
                let output = out.as_slice().to_vec();
                let req = NewOrder {
                    client_order_id: ClientOrderId(2),
                    order_type: kind,
                    side,
                    price: 100,
                    quantity: 3,
                };
                assert_eq!(book.submit(&req, &mut out, 0), Err(Error::TradeBufferFull));
                assert_eq!((*book.get(a).unwrap(), *book.get(b).unwrap()), before);
                assert_eq!(out.as_slice(), output);
                assert_eq!(book.live_orders(), 2);
                // A retry must trade exactly once against each maker.
                let mut retry = TradeBuf::with_capacity(2);
                book.submit(&req, &mut retry, 0).unwrap();
                assert_eq!(retry.len(), 2);
                assert_eq!(retry.as_slice()[0].trade_id.0, 1);
                assert_eq!(book.get(b).unwrap().remaining_qty, 1);
                assert_eq!(book.live_orders(), 1);
            }
        }
    }
}

#[test]
fn full_output_still_allows_non_crossing_orders_and_killed_fok() {
    let mut book = book();
    let mut out = TradeBuf::with_capacity(0);
    add(&mut book, &mut out, Side::Sell, 100, 1);
    add(&mut book, &mut out, Side::Buy, 99, 1);
    let mut req = NewOrder::limit(ClientOrderId(2), Side::Buy, 100, 2);
    req.order_type = OrderType::FillOrKill;
    let id = book.submit(&req, &mut out, 0).unwrap();
    assert!(book.get(id).is_none());
    assert_eq!(book.live_orders(), 2);
}

#[test]
fn amend_reduction_preserves_executed_quantity_and_fifo() {
    for side in [Side::Buy, Side::Sell] {
        let mut book = book();
        let mut out = TradeBuf::with_capacity(4);
        let first = add(&mut book, &mut out, side, 100, 10);
        let second = add(&mut book, &mut out, side, 100, 10);
        let seq = book.get(first).unwrap().seq;
        book.submit(
            &NewOrder::market(ClientOrderId(2), side.opposite(), 4),
            &mut out,
            0,
        )
        .unwrap();
        book.amend(first, 100, 3, &mut out, 0).unwrap();
        let slot = book.get(first).unwrap();
        assert_eq!(
            (slot.remaining_qty, slot.executed_qty(), slot.seq),
            (3, 4, seq)
        );
        out.clear();
        book.submit(
            &NewOrder::market(ClientOrderId(3), side.opposite(), 4),
            &mut out,
            0,
        )
        .unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(book.get(second).unwrap().remaining_qty, 9);
        assert!(book.get(first).is_none());
    }
}

#[test]
fn amend_increase_loses_fifo_and_zero_cancels() {
    let mut book = book();
    let mut out = TradeBuf::with_capacity(4);
    let first = add(&mut book, &mut out, Side::Buy, 100, 2);
    let second = add(&mut book, &mut out, Side::Buy, 100, 2);
    book.amend(first, 100, 3, &mut out, 0).unwrap();
    assert!(book.get(first).unwrap().seq > book.get(second).unwrap().seq);
    book.submit(
        &NewOrder::market(ClientOrderId(2), Side::Sell, 2),
        &mut out,
        0,
    )
    .unwrap();
    assert_eq!(out.as_slice()[0].bid_order_id, second);
    book.amend(first, i64::MIN, 0, &mut out, 0).unwrap();
    assert_eq!(book.live_orders(), 0);
    assert_eq!(book.get_best_bid(), None);
    assert_eq!(
        book.amend(first, 100, 1, &mut out, 0),
        Err(Error::OrderNotFound { order_id: first })
    );
}

#[test]
fn reprice_can_match_and_rest_and_rejection_preserves_original() {
    let mut book = book();
    let mut out = TradeBuf::with_capacity(0);
    let bid = add(&mut book, &mut out, Side::Buy, 99, 4);
    let ask = add(&mut book, &mut out, Side::Sell, 100, 2);
    let original = *book.get(bid).unwrap();
    assert_eq!(
        book.amend(bid, 100, 4, &mut out, 0),
        Err(Error::TradeBufferFull)
    );
    assert_eq!(*book.get(bid).unwrap(), original);
    assert_eq!(
        book.amend(bid, 256, 4, &mut out, 0),
        Err(Error::PriceNotOnLadder { price: 256 })
    );
    assert_eq!(*book.get(bid).unwrap(), original);
    let mut out = TradeBuf::with_capacity(1);
    book.amend(bid, 100, 4, &mut out, 0).unwrap();
    assert!(book.get(ask).is_none());
    assert_eq!(out.as_slice()[0].bid_order_id, bid);
    assert_eq!(book.get(bid).unwrap().remaining_qty, 2);
    assert_eq!(book.get(bid).unwrap().executed_qty(), 2);
    assert_eq!(book.best_bid_level().unwrap().volume, 2);
    assert_eq!(book.level_count(), 1);
}

#[test]
fn volume_overflow_is_rejected_and_fok_sum_does_not_wrap() {
    let mut book = book();
    let mut out = TradeBuf::with_capacity(4);
    let first = add(&mut book, &mut out, Side::Sell, 100, u64::MAX - 1);
    let second = add(&mut book, &mut out, Side::Sell, 100, 1);
    assert_eq!(
        book.submit(
            &NewOrder::limit(ClientOrderId(2), Side::Sell, 100, 1),
            &mut out,
            0
        ),
        Err(Error::QuantityOverflow)
    );
    assert_eq!(
        book.amend(second, 100, 2, &mut out, 0),
        Err(Error::QuantityOverflow)
    );
    assert_eq!(book.get(second).unwrap().remaining_qty, 1);
    assert_eq!(book.best_ask_level().unwrap().volume, u64::MAX);
    add(&mut book, &mut out, Side::Sell, 101, 1);
    let req = NewOrder {
        client_order_id: ClientOrderId(3),
        order_type: OrderType::FillOrKill,
        side: Side::Buy,
        price: 101,
        quantity: u64::MAX,
    };
    book.submit(&req, &mut out, 0).unwrap();
    assert_eq!(out.len(), 2);
    assert!(book.get(first).is_none());
    assert!(book.get(second).is_none());
    assert_eq!(book.best_ask_level().unwrap().volume, 1);
}

#[test]
fn amend_total_overflow_keeps_partially_filled_order() {
    let mut book = book();
    let mut out = TradeBuf::with_capacity(1);
    let id = add(&mut book, &mut out, Side::Buy, 100, 2);
    book.submit(
        &NewOrder::market(ClientOrderId(2), Side::Sell, 1),
        &mut out,
        0,
    )
    .unwrap();
    let before = *book.get(id).unwrap();
    assert_eq!(
        book.amend(id, 100, u64::MAX, &mut out, 0),
        Err(Error::QuantityOverflow)
    );
    assert_eq!(*book.get(id).unwrap(), before);
}

#[test]
fn deterministic_mixed_commands_match_a_simple_reference_book() {
    #[derive(Clone, Copy)]
    struct Resting {
        id: OrderId,
        side: Side,
        price: i64,
        qty: u64,
        priority: u64,
    }

    // Intentionally slow, flat reference: sort by price/priority for every fill.
    fn execute(
        orders: &mut Vec<Resting>,
        mut incoming: Resting,
    ) -> Vec<(OrderId, OrderId, i64, u64)> {
        let mut trades = Vec::new();
        while incoming.qty > 0 {
            let maker = orders
                .iter()
                .enumerate()
                .filter(|(_, o)| {
                    o.side != incoming.side
                        && match incoming.side {
                            Side::Buy => o.price <= incoming.price,
                            Side::Sell => o.price >= incoming.price,
                        }
                })
                .min_by_key(|(_, o)| {
                    (
                        if incoming.side == Side::Buy {
                            o.price
                        } else {
                            -o.price
                        },
                        o.priority,
                    )
                })
                .map(|(i, _)| i);
            let Some(i) = maker else { break };
            let qty = incoming.qty.min(orders[i].qty);
            let (bid, ask) = if incoming.side == Side::Buy {
                (incoming.id, orders[i].id)
            } else {
                (orders[i].id, incoming.id)
            };
            trades.push((bid, ask, orders[i].price, qty));
            incoming.qty -= qty;
            orders[i].qty -= qty;
            if orders[i].qty == 0 {
                orders.swap_remove(i);
            }
        }
        if incoming.qty > 0 {
            orders.push(incoming);
        }
        trades
    }

    let mut book = book();
    let mut out = TradeBuf::with_capacity(32);
    let mut model: Vec<Resting> = Vec::new();
    let mut seed = 42u64;
    for priority in 1..=5000 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let random = seed >> 16;
        let side = if random & 1 == 0 {
            Side::Buy
        } else {
            Side::Sell
        };
        let price = 95 + (random % 11) as i64;
        let qty = 1 + random % 7;
        out.clear();
        let expected = if !model.is_empty() && (model.len() >= 30 || random.is_multiple_of(5)) {
            let i = random as usize % model.len();
            book.cancel(model.swap_remove(i).id).unwrap();
            Vec::new()
        } else if !model.is_empty() && random.is_multiple_of(3) {
            let i = random as usize % model.len();
            let old = model[i];
            book.amend(old.id, price, qty, &mut out, 0).unwrap();
            if price == old.price && qty <= old.qty {
                model[i].qty = qty;
                Vec::new()
            } else {
                model.swap_remove(i);
                execute(
                    &mut model,
                    Resting {
                        price,
                        qty,
                        priority,
                        ..old
                    },
                )
            }
        } else {
            let id = add(&mut book, &mut out, side, price, qty);
            execute(
                &mut model,
                Resting {
                    id,
                    side,
                    price,
                    qty,
                    priority,
                },
            )
        };
        let actual: Vec<_> = out
            .as_slice()
            .iter()
            .map(|t| (t.bid_order_id, t.ask_order_id, t.price, t.quantity))
            .collect();
        assert_eq!(actual, expected, "command {priority}");
        assert_eq!(book.live_orders() as usize, model.len());
        for order in &model {
            let slot = book.get(order.id).unwrap();
            assert_eq!(
                (slot.side, slot.price, slot.remaining_qty),
                (order.side, order.price, order.qty)
            );
        }
        for side in [Side::Buy, Side::Sell] {
            let mut levels = std::collections::BTreeMap::new();
            for order in model.iter().filter(|o| o.side == side) {
                let level = levels.entry(order.price).or_insert((0u64, 0u32));
                level.0 += order.qty;
                level.1 += 1;
            }
            let mut depth = [orderbook::LevelInfo {
                price: 0,
                volume: 0,
                order_count: 0,
            }; 32];
            let n = match side {
                Side::Buy => book.bid_depth(&mut depth),
                Side::Sell => book.ask_depth(&mut depth),
            };
            let mut expected: Vec<_> = levels.into_iter().map(|(p, (v, n))| (p, v, n)).collect();
            if side == Side::Buy {
                expected.reverse();
            }
            assert_eq!(
                depth[..n]
                    .iter()
                    .map(|l| (l.price, l.volume, l.order_count))
                    .collect::<Vec<_>>(),
                expected
            );
        }
    }
}

use std::sync::{Arc, Mutex};
use tracing::{info, warn, error};
use polymarket_client_sdk::clob::types::{OrderType, Side as PolySide};
use polymarket_client_sdk::types::Decimal;
use crate::models::MarketPrices;
use super::strat::{RealEngine, TradingState};

/// Размещаем лимитку второй ноги по актуальному best_bid
pub fn place_second_leg(engine: &RealEngine, first_leg_is_up: bool, trading_state: &Arc<Mutex<TradingState>>, stream_name: &str) {
    let remaining_size = {
        let state = trading_state.lock().unwrap();
        if let TradingState::SearchingSecondLeg { second_leg_filled, .. } = *state {
            engine.config.size - second_leg_filled
        } else {
            engine.config.size
        }
    };

    if remaining_size < 5.0 {
        warn!("⚠️ {} | Размер второй ноги < 5.0 ({:.2}), пропускаем размещение", stream_name, remaining_size);
        return;
    }

    let second_leg_price = {
        let prices_opt = engine.last_prices.lock().unwrap();
        if let Some(ref prices) = *prices_opt {
            let bid = if first_leg_is_up {
                prices.down_bid
            } else {
                prices.up_bid
            };
            RealEngine::round_price(bid)
        } else {
            warn!("⚠️ {} | Актуальные цены недоступны, пропускаем размещение второй ноги", stream_name);
            return;
        }
    };

    let second_leg_is_up = !first_leg_is_up;

    if second_leg_price < 0.01 || second_leg_price > 0.99 {
        warn!("⚠️ {} | Некорректная цена второй ноги: {:.2}", stream_name, second_leg_price);
        return;
    }

    info!("📝 {} | Размещаем вторую ногу: {} @ {:.2} (best_bid) | Size: {:.2}",
        stream_name, if second_leg_is_up { "UP" } else { "DOWN" }, second_leg_price, remaining_size);

    {
        let mut state = trading_state.lock().unwrap();
        if let TradingState::SearchingSecondLeg { ref mut second_leg_current_price, .. } = *state {
            *second_leg_current_price = Some(second_leg_price);
        }
    }

    let token_id = if second_leg_is_up {
        Arc::clone(&engine.up_token)
    } else {
        Arc::clone(&engine.down_token)
    };
    let client = engine.client.clone();
    let signer = engine.signer.clone();
    let trading_state_clone = Arc::clone(trading_state);

    tokio::spawn(async move {
        let price_dec: Decimal = format!("{:.2}", second_leg_price).parse().unwrap();
        let size_dec: Decimal = format!("{:.2}", remaining_size).parse().unwrap();

        let order = client.limit_order()
            .token_id(token_id.as_ref())
            .price(price_dec)
            .size(size_dec)
            .side(PolySide::Buy)
            .order_type(OrderType::GTC)
            .build().await.unwrap();

        let signed = client.sign(&signer, order).await.unwrap();

        match client.post_order(signed).await {
            Ok(response) => {
                if !response.order_id.is_empty() {
                    info!("📝 Вторая нога размещена: order_id={}", response.order_id);

                    let mut state = trading_state_clone.lock().unwrap();
                    if let TradingState::SearchingSecondLeg {
                        first_leg_price,
                        first_leg_is_up,
                        first_leg_size,
                        second_leg_current_price,
                        second_leg_filled,
                        ..
                    } = *state {
                        *state = TradingState::SearchingSecondLeg {
                            first_leg_price,
                            first_leg_is_up,
                            first_leg_size,
                            second_leg_order_id: Some(response.order_id),
                            second_leg_current_price,
                            second_leg_filled,
                            pending_repricing: None,
                        };
                    }
                }
            },
            Err(e) => error!("❌ Ошибка размещения второй ноги: {}", e),
        }
    });
}

/// Размещает вторую ногу с указанными ценой и размером
pub fn place_second_leg_with_price(
    engine: &RealEngine,
    first_leg_is_up: bool,
    trading_state: &Arc<Mutex<TradingState>>,
    stream_name: &str,
    price: f64,
    size: f64
) {
    let second_leg_is_up = !first_leg_is_up;

    if price < 0.01 || price > 0.99 {
        warn!("⚠️ {} | Некорректная цена второй ноги: {:.2}", stream_name, price);
        return;
    }

    info!("📝 {} | Размещаем вторую ногу: {} @ {:.2} | Size: {:.2}",
        stream_name, if second_leg_is_up { "UP" } else { "DOWN" }, price, size);

    {
        let mut state = trading_state.lock().unwrap();
        if let TradingState::SearchingSecondLeg { ref mut second_leg_current_price, .. } = *state {
            *second_leg_current_price = Some(price);
        }
    }

    let token_id = if second_leg_is_up {
        Arc::clone(&engine.up_token)
    } else {
        Arc::clone(&engine.down_token)
    };
    let client = engine.client.clone();
    let signer = engine.signer.clone();
    let trading_state_clone = Arc::clone(trading_state);

    tokio::spawn(async move {
        let price_dec: Decimal = format!("{:.2}", price).parse().unwrap();
        let size_dec: Decimal = format!("{:.2}", size).parse().unwrap();

        let order = client.limit_order()
            .token_id(token_id.as_ref())
            .price(price_dec)
            .size(size_dec)
            .side(PolySide::Buy)
            .order_type(OrderType::GTC)
            .build().await.unwrap();

        let signed = client.sign(&signer, order).await.unwrap();

        match client.post_order(signed).await {
            Ok(response) => {
                if !response.order_id.is_empty() {
                    info!("📝 Вторая нога размещена после WebSocket CANCELLATION: order_id={}", response.order_id);

                    let mut state = trading_state_clone.lock().unwrap();
                    if let TradingState::SearchingSecondLeg {
                        first_leg_price,
                        first_leg_is_up,
                        first_leg_size,
                        second_leg_current_price,
                        second_leg_filled,
                        ..
                    } = *state {
                        *state = TradingState::SearchingSecondLeg {
                            first_leg_price,
                            first_leg_is_up,
                            first_leg_size,
                            second_leg_order_id: Some(response.order_id),
                            second_leg_current_price,
                            second_leg_filled,
                            pending_repricing: None,
                        };
                    }
                }
            },
            Err(e) => error!("❌ Ошибка размещения второй ноги после CANCELLATION: {}", e),
        }
    });
}

/// Проверяем возможность перевыставления второй ноги если best_bid изменился
pub fn check_second_leg_repricing(engine: &Arc<RealEngine>, prices: &MarketPrices) {
    // Проверяем UP-потоки
    for (thread_idx, trading_state) in engine.up_threads.iter().enumerate() {
        let stream_name = format!("UP-поток #{}", thread_idx + 1);
        check_second_leg_repricing_for_stream(engine, trading_state, prices, &stream_name);
    }
    // Проверяем DOWN-потоки
    for (thread_idx, trading_state) in engine.down_threads.iter().enumerate() {
        let stream_name = format!("DOWN-поток #{}", thread_idx + 1);
        check_second_leg_repricing_for_stream(engine, trading_state, prices, &stream_name);
    }
}

/// Проверяем перевыставление второй ноги для конкретного потока
fn check_second_leg_repricing_for_stream(
    engine: &Arc<RealEngine>,
    trading_state: &Arc<Mutex<TradingState>>,
    prices: &MarketPrices,
    stream_name: &str,
) {
    let state = trading_state.lock().unwrap();

    if let TradingState::SearchingSecondLeg {
        first_leg_is_up,
        second_leg_order_id: Some(ref order_id),
        second_leg_current_price: Some(current_price),
        second_leg_filled,
        ..
    } = *state
    {
        let new_best_bid = if first_leg_is_up {
            prices.down_bid
        } else {
            prices.up_bid
        };

        let new_best_bid = RealEngine::round_price(new_best_bid);
        let price_diff = new_best_bid - current_price;

        if price_diff >= 0.01 {
            let remaining_size = engine.config.size - second_leg_filled;

            if remaining_size < 0.01 {
                return;
            }

            info!("🔄 {} | Best_bid ПОВЫСИЛСЯ: {:.2} → {:.2} (+{:.2})",
                stream_name, current_price, new_best_bid, price_diff);
            info!("   Отменяем текущий ордер и перевыставляем с size={:.2}", remaining_size);

            let order_id_to_cancel = order_id.clone();
            let stream_name_owned = stream_name.to_string();
            let trading_state_clone = Arc::clone(trading_state);
            let client = engine.client.clone();

            drop(state);

            tokio::spawn(async move {
                info!("🚫 {} | Отменяем ордер для перевыставления: {}", stream_name_owned, order_id_to_cancel);

                let cancel_success = match client.cancel_order(&order_id_to_cancel).await {
                    Ok(result) => {
                        if !result.canceled.is_empty() {
                            info!("✅ Ордер отменён: {}", order_id_to_cancel);
                            true
                        } else {
                            warn!("⚠️ Ордер не был отменён: {}", order_id_to_cancel);
                            false
                        }
                    },
                    Err(e) => {
                        error!("❌ Ошибка отмены ордера {}: {}", order_id_to_cancel, e);
                        false
                    }
                };

                if cancel_success {
                    {
                        let mut state = trading_state_clone.lock().unwrap();
                        if let TradingState::SearchingSecondLeg {
                            first_leg_price,
                            first_leg_is_up,
                            first_leg_size,
                            second_leg_filled,
                            ..
                        } = *state {
                            info!("⏳ {} | Ордер отменен API, ждем WebSocket CANCELLATION для перевыставления @ {:.2}", stream_name_owned, new_best_bid);
                            *state = TradingState::SearchingSecondLeg {
                                first_leg_price,
                                first_leg_is_up,
                                first_leg_size,
                                second_leg_order_id: Some(order_id_to_cancel),
                                second_leg_current_price: None,
                                second_leg_filled,
                                pending_repricing: Some(new_best_bid),
                            };
                        }
                    }
                } else {
                    warn!("⚠️ {} | Отмена не удалась, пропускаем перевыставление", stream_name_owned);
                }
            });
        }
    }
}

/// Обновляем second_leg_filled при UPDATE события второй ноги
pub fn update_second_leg_filled(engine: &RealEngine, order_id: &str, size: f64) {
    for trading_state in engine.up_threads.iter() {
        update_second_leg_filled_for_stream(trading_state, order_id, size);
    }
    for trading_state in engine.down_threads.iter() {
        update_second_leg_filled_for_stream(trading_state, order_id, size);
    }
}

fn update_second_leg_filled_for_stream(
    trading_state: &Arc<Mutex<TradingState>>,
    order_id: &str,
    size: f64,
) {
    let mut state = trading_state.lock().unwrap();

    if let TradingState::SearchingSecondLeg {
        second_leg_order_id: Some(ref second_order_id),
        ref mut second_leg_filled,
        ..
    } = *state
    {
        if order_id == second_order_id {
            *second_leg_filled += size;
        }
    }
}

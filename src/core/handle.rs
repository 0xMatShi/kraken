use std::sync::{Arc, Mutex};
use tracing::{info, warn};
use polymarket_client_sdk::clob::types::Side as PolySide;
use chrono::Utc;
use uuid::Uuid;
use crate::ui::{self, TradeHistoryEntry, TradeType, OpenOrder};
use super::strat::{RealEngine, TradingState, price_to_cents, ReservedPriceKey};

/// Обработка TAKER сделок (market orders FAK)
pub fn handle_ws_trade(
    engine: &Arc<RealEngine>,
    trade_id: String,
    price: f64,
    size: f64,
    side: PolySide,
    asset_id: &str,
    trade_owner: Option<Uuid>,
    taker_order_id: Option<String>,
) {
    let owner_matches = trade_owner.map_or(false, |owner| owner == engine.our_api_key);
    if !owner_matches { return; }

    {
        let mut seen = engine.seen_trades.lock().unwrap();
        if seen.contains(&trade_id) { return; }
        seen.insert(trade_id.clone());
    }

    let mut port = engine.portfolio.lock().unwrap();
    port.taker_trades += 1;

    let is_buy = matches!(side, PolySide::Buy);

    if asset_id == &*engine.up_token {
        if is_buy {
            port.up_shares += size;
            port.up_spent += price * size;
            port.up_total_placed += size;
        } else {
            port.up_shares -= size;
            port.up_spent -= price * size;
        }
    } else if asset_id == &*engine.down_token {
        if is_buy {
            port.down_shares += size;
            port.down_spent += price * size;
            port.down_total_placed += size;
        } else {
            port.down_shares -= size;
            port.down_spent -= price * size;
        }
    }

    let side_str = if is_buy { "BUY" } else { "SELL" };
    let token_str = if asset_id == &*engine.up_token { "UP" } else { "DOWN" };
    let is_up = asset_id == &*engine.up_token;

    info!("✅ TAKER FILLED: {} {} @ {:.3} | Size: {:.2} | Cost: ${:.2}",
        side_str, token_str, price, size, price * size);
    info!("💰 Portfolio: UP {:.1} | DOWN {:.1} | Skew {:.1}",
        port.up_shares, port.down_shares, port.up_shares - port.down_shares);

    drop(port);

    if is_buy {
        let history_entry = TradeHistoryEntry {
            is_up,
            shares: size,
            price,
            cost: price * size,
            trade_type: TradeType::Taker,
            timestamp: Utc::now(),
        };
        ui::add_trade_history(&engine.ui_state, history_entry);
    }

    engine.update_ui_portfolio();

    if is_buy {
        if let Some(order_id) = taker_order_id {
            let engine_clone = Arc::clone(engine);
            tokio::spawn(async move {
                handle_taker_fill_for_strategy(&engine_clone, &order_id, is_up, price, size).await;
            });
        }
    }
}

/// Обработка taker fill в контексте стратегии
async fn handle_taker_fill_for_strategy(engine: &RealEngine, taker_order_id: &str, is_up: bool, price: f64, size: f64) {
    loop {
        let mut matched = false;

        // Проверяем первую ногу
        for (thread_idx, trading_state) in engine.up_threads.iter().enumerate() {
            if matched { break; }
            let stream_name = format!("UP-поток #{}", thread_idx + 1);
            matched = check_first_leg_fill_for_stream(engine, trading_state, taker_order_id, is_up, price, size, &stream_name);
        }
        if !matched {
            for (thread_idx, trading_state) in engine.down_threads.iter().enumerate() {
                if matched { break; }
                let stream_name = format!("DOWN-поток #{}", thread_idx + 1);
                matched = check_first_leg_fill_for_stream(engine, trading_state, taker_order_id, is_up, price, size, &stream_name);
            }
        }

        // Проверяем вторую ногу (может исполниться как TAKER если пересекает спред)
        if !matched {
            for (thread_idx, trading_state) in engine.up_threads.iter().enumerate() {
                if matched { break; }
                let stream_name = format!("UP-поток #{}", thread_idx + 1);
                matched = check_second_leg_taker_fill_for_stream(engine, trading_state, taker_order_id, is_up, price, size, &stream_name);
            }
        }
        if !matched {
            for (thread_idx, trading_state) in engine.down_threads.iter().enumerate() {
                if matched { break; }
                let stream_name = format!("DOWN-поток #{}", thread_idx + 1);
                matched = check_second_leg_taker_fill_for_stream(engine, trading_state, taker_order_id, is_up, price, size, &stream_name);
            }
        }

        // Проверяем хедж (может исполниться как TAKER если пересекает спред)
        if !matched {
            matched = check_hedge_taker_fill(engine, taker_order_id, is_up, price, size);
        }

        // Если нашли match → выходим из loop
        if matched {
            break;
        }

        // Не нашли match → ждем 5ms и пытаемся снова
        tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
    }
}

/// Проверяем заполнение первой ноги для конкретного потока
fn check_first_leg_fill_for_stream(
    engine: &RealEngine,
    trading_state: &Arc<Mutex<TradingState>>,
    taker_order_id: &str,
    is_up: bool,
    price: f64,
    size: f64,
    stream_name: &str,
) -> bool {
    let mut state = trading_state.lock().unwrap();

    match &*state {
        TradingState::WaitingFirstLeg { order_id: first_leg_order_id, is_up: expected_is_up, price: expected_price, size: expected_size } => {
            let is_our_first_leg = if first_leg_order_id.is_empty() {
                let matches = is_up == *expected_is_up &&
                    (price - *expected_price).abs() < 0.02 &&
                    (size - *expected_size).abs() < 0.01;
                if matches {
                    info!("🔄 {} | TAKER FILL распознан по атрибутам (order_id ещё не получен)", stream_name);
                }
                matches
            } else {
                taker_order_id == first_leg_order_id
            };

            if is_our_first_leg {
                info!("🔄 {} | TAKER FILL = ПЕРВАЯ НОГА! order_id={}", stream_name, taker_order_id);
                info!("   {} @ {:.2} size={:.2} → SearchingSecondLeg",
                    if is_up { "UP" } else { "DOWN" }, price, size);

                let first_leg_price = price;
                let first_leg_is_up = is_up;
                let first_leg_size = size;

                *state = TradingState::SearchingSecondLeg {
                    first_leg_price,
                    first_leg_is_up,
                    first_leg_size,
                    second_leg_order_id: None,
                    second_leg_current_price: None,
                    second_leg_filled: 0.0,
                    pending_repricing: None,
                    api_cancel_confirmed: false,
                    websocket_cancel_confirmed: false,
                };
                drop(state);

                super::second_leg::place_second_leg(engine, first_leg_is_up, trading_state, stream_name);
                return true;
            }
        }
        _ => {}
    }
    false
}

/// Проверяем заполнение второй ноги как TAKER для конкретного потока
fn check_second_leg_taker_fill_for_stream(
    engine: &RealEngine,
    trading_state: &Arc<Mutex<TradingState>>,
    taker_order_id: &str,
    is_up: bool,
    price: f64,
    size: f64,
    stream_name: &str,
) -> bool {
    let mut state = trading_state.lock().unwrap();

    if let TradingState::SearchingSecondLeg {
        first_leg_is_up,
        second_leg_order_id,
        second_leg_filled,
        ..
    } = &*state {
        // Вторая нога должна быть на противоположной стороне от первой
        let expected_second_leg_is_up = !first_leg_is_up;

        let is_our_second_leg = if let Some(second_order_id) = second_leg_order_id {
            // Если есть order_id - проверяем по нему
            taker_order_id == second_order_id
        } else {
            // Если нет order_id - распознаем по стороне (противоположной первой)
            is_up == expected_second_leg_is_up
        };

        if is_our_second_leg {
            info!("🔄 {} | TAKER FILL = ВТОРАЯ НОГА! order_id={}", stream_name, taker_order_id);
            info!("   {} @ {:.2} size={:.2}",
                if is_up { "UP" } else { "DOWN" }, price, size);

            let new_filled = second_leg_filled + size;
            let target_size = engine.config.size;

            // Обновляем second_leg_filled
            if let TradingState::SearchingSecondLeg {
                ref mut second_leg_filled,
                ..
            } = *state {
                *second_leg_filled = new_filled;
            }

            // Проверяем: заполнена ли вторая нога полностью?
            if (target_size - new_filled).abs() < 0.01 || new_filled >= target_size {
                info!("✅ {} | Вторая нога ПОЛНОСТЬЮ ЗАПОЛНЕНА через TAKER ({:.2}/{:.2}) → возвращаемся в Idle",
                    stream_name, new_filled, target_size);
                *state = TradingState::Idle;
            } else {
                info!("📊 {} | Вторая нога частично заполнена: {:.2}/{:.2}",
                    stream_name, new_filled, target_size);
            }
            return true;
        }
    }
    false
}

/// Проверяем заполнение хеджа как TAKER
fn check_hedge_taker_fill(
    engine: &RealEngine,
    taker_order_id: &str,
    is_up: bool,
    price: f64,
    size: f64,
) -> bool {
    let mut hedge = engine.hedge_state.lock().unwrap();

    if let Some(ref state) = *hedge {
        if let Some(ref hedge_order_id) = state.order_id {
            // Проверяем: это наш хедж?
            let is_our_hedge = taker_order_id == hedge_order_id && is_up == state.is_up_side;

            if is_our_hedge {
                info!("🔄 ХЕДЖ | TAKER FILL! order_id={}", taker_order_id);
                info!("   {} @ {:.2} size={:.2}",
                    if is_up { "UP" } else { "DOWN" }, price, size);

                let new_filled = state.filled_size + size;
                let target_size = state.target_size;

                // Обновляем filled_size
                if let Some(ref mut hedge_state) = *hedge {
                    hedge_state.filled_size = new_filled;
                }

                // Проверяем: заполнен ли хедж полностью?
                if (target_size - new_filled).abs() < 0.01 || new_filled >= target_size {
                    info!("✅ ХЕДЖ | ПОЛНОСТЬЮ ЗАПОЛНЕН через TAKER ({:.2}/{:.2}) → сбрасываем состояние",
                        new_filled, target_size);
                    *hedge = None;
                    *engine.hedging_active.lock().unwrap() = false;
                    info!("🔓 Потоки разблокированы");
                } else {
                    info!("📊 ХЕДЖ | Частично заполнен: {:.2}/{:.2}",
                        new_filled, target_size);
                }
                return true;
            }
        }
    }
    false
}

/// Обновляем PLACEMENT для хеджа
fn update_hedge_placement(
    engine: &RealEngine,
    order_id: &str,
    is_up: bool,
    price: f64,
    token_str: &str,
) {
    let hedge = engine.hedge_state.lock().unwrap();

    if let Some(ref state) = *hedge {
        if let Some(ref hedge_order_id) = state.order_id {
            if hedge_order_id == order_id && is_up == state.is_up_side {
                // Если отмена уже в процессе (pending_repricing установлен) → игнорируем PLACEMENT
                if state.pending_repricing.is_some() {
                    info!("⚠️ ХЕДЖ | Игнорируем PLACEMENT {} @ {:.2} - отмена уже в процессе", token_str, price);
                    return;
                }

                info!("📝 ХЕДЖ | PLACEMENT подтверждён: {} @ {:.2}", token_str, price);
                // НЕ перезаписываем state, так как pending_repricing = None (нет активной отмены)
            }
        }
    }
}

/// Обновляем placement для конкретного потока
fn update_placement_for_stream(
    trading_state: &Arc<Mutex<TradingState>>,
    order_id: &str,
    price: f64,
    token_str: &str,
    is_first_leg: bool,
) -> bool {
    let mut state = trading_state.lock().unwrap();

    if is_first_leg {
        if let TradingState::WaitingFirstLeg { order_id: ref mut first_order_id, .. } = *state {
            if first_order_id.is_empty() {
                info!("🎯 Первая нога подтверждена: {} @ {:.2}", token_str, price);
                *first_order_id = order_id.to_string();
                return true;
            }
        }
    } else {
        if let TradingState::SearchingSecondLeg {
            first_leg_price,
            first_leg_is_up,
            first_leg_size,
            second_leg_order_id: Some(ref existing_order_id),
            second_leg_filled,
            pending_repricing,
            api_cancel_confirmed,
            websocket_cancel_confirmed,
            ..
        } = *state {
            if existing_order_id == order_id {
                // Если отмена уже в процессе (pending_repricing установлен) → игнорируем PLACEMENT
                if pending_repricing.is_some() {
                    info!("⚠️ Игнорируем PLACEMENT {} @ {:.2} - отмена уже в процессе", token_str, price);
                    return false;
                }

                info!("📝 Вторая нога PLACEMENT подтверждён (order_id match): {} @ {:.2}", token_str, price);
                *state = TradingState::SearchingSecondLeg {
                    first_leg_price,
                    first_leg_is_up,
                    first_leg_size,
                    second_leg_order_id: Some(order_id.to_string()),
                    second_leg_current_price: Some(price),
                    second_leg_filled,
                    pending_repricing,              // Сохраняем!
                    api_cancel_confirmed,           // Сохраняем!
                    websocket_cancel_confirmed,     // Сохраняем!
                };
                return true;
            }
        }
    }

    false
}

/// Обработка событий MAKER ордеров (PLACEMENT, UPDATE, CANCELLATION)
pub fn handle_ws_order(
    engine: &Arc<RealEngine>,
    order_id: String,
    msg_type: Option<String>,
    price: f64,
    side: PolySide,
    asset_id: &str,
    size_matched: Option<f64>,
    original_size: Option<f64>
) {
    // Дедупликация для PLACEMENT и CANCELLATION
    if msg_type.as_deref() != Some("UPDATE") {
        let order_key = format!("{}:{:?}", order_id, msg_type);
        let mut seen = engine.seen_orders.lock().unwrap();
        if seen.contains(&order_key) { return; }
        seen.insert(order_key);
    }

    let side_str = match side {
        PolySide::Buy => "BUY",
        PolySide::Sell => "SELL",
        _ => "UNKNOWN",
    };

    let token_str = if asset_id == &*engine.up_token { "UP" } else { "DOWN" };

    match msg_type.as_deref() {
        Some("PLACEMENT") => {
            engine.add_order_id(order_id.clone());

            let is_up = asset_id == &*engine.up_token;

            if let Some(size) = original_size {
                let mut orders_info = engine.active_orders_info.lock().unwrap();
                orders_info.insert(order_id.clone(), (price, is_up, size, 0.0));

                let open_order = OpenOrder {
                    order_id: order_id.clone(),
                    is_up,
                    price,
                    filled: 0.0,
                    total: size,
                };
                ui::add_open_order(&engine.ui_state, open_order);
            }

            ui::add_our_bid_price(&engine.ui_state, is_up, price);

            if let Some(size) = original_size {
                let mut port = engine.portfolio.lock().unwrap();
                if is_up {
                    port.up_total_placed += size;
                } else {
                    port.down_total_placed += size;
                }
                drop(port);
                engine.update_ui_portfolio();
            }

            info!("📝 MAKER PLACED: {} {} @ {:.3}", side_str, token_str, price);

            let mut placement_handled = false;

            for trading_state in engine.up_threads.iter() {
                if placement_handled { break; }

                let state = trading_state.lock().unwrap().clone();
                match &state {
                    TradingState::WaitingFirstLeg { is_up: expected_is_up, .. } => {
                        if is_up == *expected_is_up {
                            if update_placement_for_stream(trading_state, &order_id, price, token_str, true) {
                                placement_handled = true;
                            }
                        }
                    }
                    TradingState::SearchingSecondLeg { first_leg_is_up, .. } => {
                        if is_up != *first_leg_is_up {
                            if update_placement_for_stream(trading_state, &order_id, price, token_str, false) {
                                placement_handled = true;
                            }
                        }
                    }
                    _ => {}
                }
            }

            if !placement_handled {
                for trading_state in engine.down_threads.iter() {
                    if placement_handled { break; }

                    let state = trading_state.lock().unwrap().clone();
                    match &state {
                        TradingState::WaitingFirstLeg { is_up: expected_is_up, .. } => {
                            if is_up == *expected_is_up {
                                if update_placement_for_stream(trading_state, &order_id, price, token_str, true) {
                                    placement_handled = true;
                                }
                            }
                        }
                        TradingState::SearchingSecondLeg { first_leg_is_up, .. } => {
                            if is_up != *first_leg_is_up {
                                if update_placement_for_stream(trading_state, &order_id, price, token_str, false) {
                                    placement_handled = true;
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }

            // Проверяем PLACEMENT для хеджа
            if !placement_handled {
                update_hedge_placement(engine, &order_id, is_up, price, token_str);
            }
        }
        Some("UPDATE") => {
            if let Some(size) = size_matched {
                let mut orders_info = engine.active_orders_info.lock().unwrap();

                if let Some((_order_price, _is_up, _original_size, accumulated_filled)) = orders_info.get_mut(&order_id) {
                    let previous_filled = *accumulated_filled;
                    *accumulated_filled += size;

                    drop(orders_info);
                    super::second_leg::update_second_leg_filled(engine, &order_id, size);
                    super::hedge::update_hedge_filled(engine, &order_id, size);
                    let mut orders_info = engine.active_orders_info.lock().unwrap();

                    if let Some((order_price, is_up, original_size, accumulated_filled)) = orders_info.get_mut(&order_id) {
                        let size_for_portfolio = if *accumulated_filled > *original_size {
                            (*original_size - previous_filled).max(0.0)
                        } else {
                            size
                        };

                        let current_is_up = *is_up;
                        let current_accumulated = *accumulated_filled;
                        let current_original_size = *original_size;
                        let current_order_price = *order_price;

                        info!("📊 MAKER PARTIAL FILL: {} {} @ {:.3} | Filled: {:.2}/{:.2}",
                            side_str, token_str, price, *accumulated_filled, *original_size);

                        ui::update_open_order_filled(&engine.ui_state, &order_id, current_accumulated);

                        let is_fully_filled = (current_accumulated - current_original_size).abs() < 0.01 || current_accumulated >= current_original_size;

                        if is_fully_filled {
                            let final_price = current_order_price;
                            let final_is_up = current_is_up;

                            orders_info.remove(&order_id);
                            drop(orders_info);

                            engine.remove_order_id(&order_id);
                            ui::remove_our_bid_price(&engine.ui_state, final_is_up, final_price);
                            ui::remove_open_order(&engine.ui_state, &order_id);
                            info!("🔔 ОРДЕР ПОЛНОСТЬЮ ИСПОЛНЕН: {} {} @ {:.3}", side_str, token_str, price);

                            handle_order_fully_filled(engine, &order_id, final_price, final_is_up, current_original_size);
                            super::hedge::complete_hedge(engine, &order_id);
                        } else {
                            drop(orders_info);
                        }

                        if size_for_portfolio > 0.0 {
                            let mut port = engine.portfolio.lock().unwrap();
                            port.maker_trades += 1;

                            let is_buy = matches!(side, PolySide::Buy);

                            if asset_id == &*engine.up_token {
                                if is_buy {
                                    port.up_shares += size_for_portfolio;
                                    port.up_spent += price * size_for_portfolio;
                                } else {
                                    port.up_shares -= size_for_portfolio;
                                    port.up_spent -= price * size_for_portfolio;
                                }
                            } else if asset_id == &*engine.down_token {
                                if is_buy {
                                    port.down_shares += size_for_portfolio;
                                    port.down_spent += price * size_for_portfolio;
                                } else {
                                    port.down_shares -= size_for_portfolio;
                                    port.down_spent -= price * size_for_portfolio;
                                }
                            }

                            info!("✅ MAKER FILLED: {} {} @ {:.3} | Size: {:.2} | Cost: ${:.2}",
                                side_str, token_str, price, size_for_portfolio, price * size_for_portfolio);
                            info!("💰 Portfolio: UP {:.1} | DOWN {:.1} | Skew {:.1}",
                                port.up_shares, port.down_shares, port.up_shares - port.down_shares);

                            drop(port);

                            if is_buy {
                                let history_entry = TradeHistoryEntry {
                                    is_up: current_is_up,
                                    shares: size_for_portfolio,
                                    price,
                                    cost: price * size_for_portfolio,
                                    trade_type: TradeType::Maker,
                                    timestamp: Utc::now(),
                                };
                                ui::add_trade_history(&engine.ui_state, history_entry);
                            }

                            engine.update_ui_portfolio();
                        }
                    } else {
                        drop(orders_info);
                    }
                }
            }
        }
        Some("CANCELLATION") => {
            engine.remove_order_id(&order_id);

            let order_info = {
                let mut orders_info = engine.active_orders_info.lock().unwrap();
                orders_info.remove(&order_id)
            };

            if let Some((order_price, is_up, _original_size, _accumulated)) = order_info {
                ui::remove_our_bid_price(&engine.ui_state, is_up, order_price);
            }

            ui::remove_open_order(&engine.ui_state, &order_id);

            warn!("❌ MAKER CANCELLED: {} {} @ {:.3}", side_str, token_str, price);

            handle_order_cancelled(engine, &order_id);
            super::hedge::cancel_hedge(engine, &order_id);
        }
        _ => {}
    }
}

/// Обработка полного заполнения ордера
fn handle_order_fully_filled(engine: &Arc<RealEngine>, order_id: &str, filled_price: f64, filled_is_up: bool, filled_size: f64) {
    for (thread_idx, trading_state) in engine.up_threads.iter().enumerate() {
        let stream_name = format!("UP-поток #{}", thread_idx + 1);
        handle_order_fully_filled_for_stream(
            engine, trading_state, order_id, filled_price, filled_is_up, filled_size, &stream_name
        );
    }
    for (thread_idx, trading_state) in engine.down_threads.iter().enumerate() {
        let stream_name = format!("DOWN-поток #{}", thread_idx + 1);
        handle_order_fully_filled_for_stream(
            engine, trading_state, order_id, filled_price, filled_is_up, filled_size, &stream_name
        );
    }
}

fn handle_order_fully_filled_for_stream(
    engine: &Arc<RealEngine>,
    trading_state: &Arc<Mutex<TradingState>>,
    order_id: &str,
    filled_price: f64,
    filled_is_up: bool,
    filled_size: f64,
    stream_name: &str,
) {
    let mut state = trading_state.lock().unwrap();

    match &*state {
        TradingState::WaitingFirstLeg { order_id: first_order_id, .. } => {
            if order_id == first_order_id {
                info!("✅ {} | ПЕРВАЯ НОГА ЗАПОЛНЕНА! {} @ {:.2} size={:.2}",
                    stream_name, if filled_is_up { "UP" } else { "DOWN" }, filled_price, filled_size);

                *state = TradingState::SearchingSecondLeg {
                    first_leg_price: filled_price,
                    first_leg_is_up: filled_is_up,
                    first_leg_size: filled_size,
                    second_leg_order_id: None,
                    second_leg_current_price: None,
                    second_leg_filled: 0.0,
                    pending_repricing: None,
                    api_cancel_confirmed: false,
                    websocket_cancel_confirmed: false,
                };
                drop(state);

                super::second_leg::place_second_leg(engine, filled_is_up, trading_state, stream_name);
            }
        }
        TradingState::SearchingSecondLeg { second_leg_order_id: Some(second_order_id), first_leg_price, first_leg_is_up, .. } => {
            if order_id == second_order_id {
                info!("✅ {} | ВТОРАЯ НОГА ЗАПОЛНЕНА! Пара завершена. Возвращаемся в Idle", stream_name);

                let price_key: ReservedPriceKey = (*first_leg_is_up, price_to_cents(*first_leg_price));
                {
                    let mut reserved = engine.reserved_prices.lock().unwrap();
                    reserved.remove(&price_key);
                }
                info!("🔓 {} | Цена {:.2} {} освобождена", stream_name, first_leg_price, if *first_leg_is_up { "UP" } else { "DOWN" });

                *state = TradingState::Idle;
            }
        }
        _ => {}
    }
}

/// Обработка отмены ордера
fn handle_order_cancelled(engine: &Arc<RealEngine>, order_id: &str) {
    for (thread_idx, trading_state) in engine.up_threads.iter().enumerate() {
        let stream_name = format!("UP-поток #{}", thread_idx + 1);
        handle_order_cancelled_for_stream(engine, trading_state, order_id, &stream_name);
    }
    for (thread_idx, trading_state) in engine.down_threads.iter().enumerate() {
        let stream_name = format!("DOWN-поток #{}", thread_idx + 1);
        handle_order_cancelled_for_stream(engine, trading_state, order_id, &stream_name);
    }
}

fn handle_order_cancelled_for_stream(
    engine: &Arc<RealEngine>,
    trading_state: &Arc<Mutex<TradingState>>,
    order_id: &str,
    stream_name: &str,
) {
    let mut state = trading_state.lock().unwrap();

    match &*state {
        TradingState::WaitingFirstLeg { order_id: first_order_id, price, is_up, .. } => {
            if order_id == first_order_id {
                let price_key: ReservedPriceKey = (*is_up, price_to_cents(*price));
                {
                    let mut reserved = engine.reserved_prices.lock().unwrap();
                    reserved.remove(&price_key);
                }
                info!("⚠️ {} | Первая нога отменена. Возвращаемся в Idle", stream_name);
                *state = TradingState::Idle;
            }
        }
        TradingState::SearchingSecondLeg {
            second_leg_order_id: Some(second_order_id),
            first_leg_price,
            first_leg_is_up,
            first_leg_size,
            second_leg_filled,
            pending_repricing,
            api_cancel_confirmed,
            ..
        } => {
            if order_id == second_order_id {
                info!("⚠️ {} | Вторая нога отменена (WebSocket CANCELLATION)", stream_name);

                let pending_price = *pending_repricing;
                let first_leg_price_val = *first_leg_price;
                let first_leg_is_up_val = *first_leg_is_up;
                let first_leg_size_val = *first_leg_size;
                let second_leg_filled_val = *second_leg_filled;
                let api_already_confirmed = *api_cancel_confirmed;

                // КЛЮЧ 2: WebSocket подтвердил отмену
                // Проверяем: если API уже подтвердил (Ключ 1) → размещаем новый ордер
                let should_place_new_order = if let Some(_new_price) = pending_price {
                    if api_already_confirmed {
                        info!("🔑 {} | КЛЮЧ 2/2 ПОВЕРНУТ (WebSocket)", stream_name);
                        info!("🔓 {} | ОБА КЛЮЧА ПОВЕРНУТЫ! Размещаем новый ордер", stream_name);
                        true
                    } else {
                        info!("🔑 {} | КЛЮЧ 2/2 ПОВЕРНУТ (WebSocket)", stream_name);
                        info!("⏳ {} | Ждём КЛЮЧ 1/2 (API confirmation)", stream_name);
                        false
                    }
                } else {
                    // Нет pending_repricing → это обычная отмена (не перевыставление)
                    info!("⚠️ {} | Обычная отмена (не перевыставление), возвращаемся в Idle", stream_name);
                    *state = TradingState::Idle;
                    return;
                };

                if should_place_new_order {
                    // Оба ключа повернуты → сбрасываем pending и размещаем новый ордер
                    *state = TradingState::SearchingSecondLeg {
                        first_leg_price: first_leg_price_val,
                        first_leg_is_up: first_leg_is_up_val,
                        first_leg_size: first_leg_size_val,
                        second_leg_order_id: None,
                        second_leg_current_price: None,
                        second_leg_filled: second_leg_filled_val,
                        pending_repricing: None,
                        api_cancel_confirmed: false,
                        websocket_cancel_confirmed: false,
                    };
                    drop(state);

                    let new_price = pending_price.unwrap();
                    let remaining_size = engine.config.size - second_leg_filled_val;

                    if remaining_size < 5.0 {
                        warn!("⚠️ {} | Размер второй ноги < 5.0 ({:.2}), пропускаем перевыставление", stream_name, remaining_size);
                        return;
                    }

                    info!("✅ {} | Размещаем новый ордер @ {:.2} | Size: {:.2}",
                        stream_name, new_price, remaining_size);

                    super::second_leg::place_second_leg_with_price(engine, first_leg_is_up_val, trading_state, stream_name, new_price, remaining_size);
                } else {
                    // WebSocket подтвердил, но API ещё нет → обновляем флаг и ждём
                    *state = TradingState::SearchingSecondLeg {
                        first_leg_price: first_leg_price_val,
                        first_leg_is_up: first_leg_is_up_val,
                        first_leg_size: first_leg_size_val,
                        second_leg_order_id: None,
                        second_leg_current_price: None,
                        second_leg_filled: second_leg_filled_val,
                        pending_repricing: pending_price,
                        api_cancel_confirmed: api_already_confirmed,
                        websocket_cancel_confirmed: true,  // Устанавливаем флаг
                    };
                }
                return;
            }
        }
        _ => {}
    }
}

use super::strat::RealEngine;
use crate::ui::{self, OpenOrder, TradeHistoryEntry, TradeType};
use chrono::Utc;
use polymarket_client_sdk::clob::types::Side as PolySide;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{info, warn};
use uuid::Uuid;

/// Обработка TAKER сделок (market orders FAK)
/// Это происходит когда наш лимитный ордер сразу исполняется как taker
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
    if !owner_matches {
        return;
    }

    {
        let mut seen = engine.seen_trades.lock().unwrap();
        if seen.contains(&trade_id) {
            return;
        }
        seen.insert(trade_id.clone());
    }

    let mut port = engine.portfolio.lock().unwrap();
    port.taker_trades += 1;

    let is_buy = matches!(side, PolySide::Buy);
    let is_up = asset_id == &*engine.up_token;

    if asset_id == &*engine.up_token {
        if is_buy {
            port.up_shares += size;
            port.up_spent += price * size;
        } else {
            port.up_shares -= size;
            port.up_spent -= price * size;
        }
    } else if asset_id == &*engine.down_token {
        if is_buy {
            port.down_shares += size;
            port.down_spent += price * size;
        } else {
            port.down_shares -= size;
            port.down_spent -= price * size;
        }
    }

    let side_str = if is_buy { "BUY" } else { "SELL" };
    let token_str = if is_up { "UP" } else { "DOWN" };

    info!(
        "✅ TAKER FILLED: {} {} @ {:.3} | Size: {:.2} | Cost: ${:.2}",
        side_str,
        token_str,
        price,
        size,
        price * size
    );
    info!(
        "💰 Portfolio: UP {:.1} | DOWN {:.1} | Skew {:.1}",
        port.up_shares,
        port.down_shares,
        port.up_shares - port.down_shares
    );

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

    // Если есть taker_order_id - запускаем асинхронный поиск и обработку ноги
    if let Some(order_id) = taker_order_id {
        let engine_clone = Arc::clone(engine);
        tokio::spawn(async move {
            match_and_process_taker_leg(&engine_clone, &order_id, size);
        });
    }
}

/// Пытается найти и обработать ногу для taker ордера
///
/// WebSocket событие о taker fill может прийти раньше, чем API ответ с order_id.
/// Поэтому в бесконечном цикле (с таймаутом) проверяем, зарегистрирована ли нога
/// с данным order_id, и если да - вызываем соответствующий обработчик.
fn match_and_process_taker_leg(engine: &Arc<RealEngine>, taker_order_id: &str, size: f64) {
    let start = Instant::now();
    let timeout = Duration::from_secs(10); // 5 секунд таймаут
    let poll_interval = Duration::from_millis(5);

    info!(
        "🔍 Начинаем поиск ноги для taker order_id: {}",
        taker_order_id
    );

    loop {
        // Проверяем таймаут
        if start.elapsed() > timeout {
            warn!(
                "⏱️ Таймаут поиска ноги для taker order_id: {}",
                taker_order_id
            );
            return;
        }

        // Проверяем, является ли это первой ногой
        let is_first_leg = {
            let pairs = engine.trade_pairs.lock().unwrap();
            pairs.contains_key(taker_order_id)
        };

        if is_first_leg {
            info!("🎯 Taker fill: найдена ПЕРВАЯ нога {}", taker_order_id);
            engine.on_first_leg_filled(taker_order_id);
            return;
        }

        // Проверяем, является ли это второй ногой
        let is_second_leg = {
            let mapping = engine.second_leg_to_first.lock().unwrap();
            mapping.contains_key(taker_order_id)
        };

        if is_second_leg {
            info!("🎯 Taker fill: найдена ВТОРАЯ нога {}", taker_order_id);
            engine.on_second_leg_filled(taker_order_id);
            return;
        }

        // Проверяем cumulative ордера
        if let Some(is_first) = engine.is_cumulative_order(taker_order_id) {
            if is_first {
                info!(
                    "🎯 Taker fill: найдена cumulative ПЕРВАЯ нога {}",
                    taker_order_id
                );
                // Для taker fill cumulative первой ноги - считаем полностью исполненной
                // (taker fill = весь ордер исполнен разом)
                engine.on_cumulative_first_leg_fill(taker_order_id, size, true);
            } else {
                info!(
                    "🎯 Taker fill: найдена cumulative ВТОРАЯ нога {}",
                    taker_order_id
                );
                // Для taker fill cumulative второй ноги - считаем полностью исполненной
                engine.on_cumulative_second_leg_fill(taker_order_id, size, true);
            }
            return;
        }

        // Ещё не зарегистрирована - ждём
        std::thread::sleep(poll_interval);
    }
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
    original_size: Option<f64>,
) {
    // Дедупликация для PLACEMENT и CANCELLATION
    if msg_type.as_deref() != Some("UPDATE") {
        let order_key = format!("{}:{:?}", order_id, msg_type);
        let mut seen = engine.seen_orders.lock().unwrap();
        if seen.contains(&order_key) {
            return;
        }
        seen.insert(order_key);
    }

    let side_str = match side {
        PolySide::Buy => "BUY",
        PolySide::Sell => "SELL",
        _ => "UNKNOWN",
    };

    let token_str = if asset_id == &*engine.up_token {
        "UP"
    } else {
        "DOWN"
    };
    let is_up = asset_id == &*engine.up_token;

    match msg_type.as_deref() {
        Some("PLACEMENT") => {
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

            info!("📝 MAKER PLACED: {} {} @ {:.3}", side_str, token_str, price);
        }
        Some("UPDATE") => {
            if let Some(size) = size_matched {
                let mut orders_info = engine.active_orders_info.lock().unwrap();

                if let Some((_order_price, _is_up, _original_size, accumulated_filled)) =
                    orders_info.get_mut(&order_id)
                {
                    let previous_filled = *accumulated_filled;
                    *accumulated_filled += size;

                    if let Some((order_price, is_up, original_size, accumulated_filled)) =
                        orders_info.get_mut(&order_id)
                    {
                        let size_for_portfolio = if *accumulated_filled > *original_size {
                            (*original_size - previous_filled).max(0.0)
                        } else {
                            size
                        };

                        let current_is_up = *is_up;
                        let current_accumulated = *accumulated_filled;
                        let current_original_size = *original_size;
                        let current_order_price = *order_price;

                        info!(
                            "📊 MAKER PARTIAL FILL: {} {} @ {:.3} | Filled: {:.2}/{:.2}",
                            side_str, token_str, price, *accumulated_filled, *original_size
                        );

                        ui::update_open_order_filled(
                            &engine.ui_state,
                            &order_id,
                            current_accumulated,
                        );

                        let is_fully_filled = (current_accumulated - current_original_size).abs()
                            < 0.01
                            || current_accumulated >= current_original_size;

                        if is_fully_filled {
                            let final_price = current_order_price;
                            let final_is_up = current_is_up;

                            orders_info.remove(&order_id);
                            drop(orders_info);

                            ui::remove_our_bid_price(&engine.ui_state, final_is_up, final_price);
                            ui::remove_open_order(&engine.ui_state, &order_id);
                            info!(
                                "🔔 ОРДЕР ПОЛНОСТЬЮ ИСПОЛНЕН: {} {} @ {:.3}",
                                side_str, token_str, price
                            );

                            // Проверяем, является ли это первой ногой
                            let is_first_leg =
                                engine.trade_pairs.lock().unwrap().contains_key(&order_id);

                            // Проверяем, является ли это второй ногой
                            let is_second_leg = engine
                                .second_leg_to_first
                                .lock()
                                .unwrap()
                                .contains_key(&order_id);

                            if is_first_leg {
                                // Первая нога исполнена - размещаем вторую
                                engine.on_first_leg_filled(&order_id);
                            } else if is_second_leg {
                                // Вторая нога исполнена - освобождаем цену
                                engine.on_second_leg_filled(&order_id);
                            }

                            // Проверяем cumulative ордера
                            if let Some(is_first) = engine.is_cumulative_order(&order_id) {
                                if is_first {
                                    engine.on_cumulative_first_leg_fill(
                                        &order_id,
                                        size_for_portfolio,
                                        true,
                                    );
                                } else {
                                    engine.on_cumulative_second_leg_fill(
                                        &order_id,
                                        size_for_portfolio,
                                        true,
                                    );
                                }
                            }
                        } else {
                            drop(orders_info);

                            // Partial fill: обновляем cumulative
                            if let Some(is_first) = engine.is_cumulative_order(&order_id) {
                                if is_first {
                                    engine.on_cumulative_first_leg_fill(
                                        &order_id,
                                        size_for_portfolio,
                                        false,
                                    );
                                } else {
                                    engine.on_cumulative_second_leg_fill(
                                        &order_id,
                                        size_for_portfolio,
                                        false,
                                    );
                                }
                            }
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

                            info!(
                                "✅ MAKER FILLED: {} {} @ {:.3} | Size: {:.2} | Cost: ${:.2}",
                                side_str,
                                token_str,
                                price,
                                size_for_portfolio,
                                price * size_for_portfolio
                            );
                            info!(
                                "💰 Portfolio: UP {:.1} | DOWN {:.1} | Skew {:.1}",
                                port.up_shares,
                                port.down_shares,
                                port.up_shares - port.down_shares
                            );

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
            let order_info = {
                let mut orders_info = engine.active_orders_info.lock().unwrap();
                orders_info.remove(&order_id)
            };

            if let Some((order_price, is_up, _original_size, _accumulated)) = order_info {
                ui::remove_our_bid_price(&engine.ui_state, is_up, order_price);
            }

            ui::remove_open_order(&engine.ui_state, &order_id);

            // Проверяем, является ли это первой ногой
            let is_first_leg = engine.trade_pairs.lock().unwrap().contains_key(&order_id);

            if is_first_leg {
                // Первая нога отменена - освобождаем цену
                engine.on_first_leg_cancelled(&order_id);
            }

            // Проверяем cumulative ордера
            if !is_first_leg {
                engine.on_cumulative_order_cancelled(&order_id);
            }

            warn!(
                "❌ MAKER CANCELLED: {} {} @ {:.3}",
                side_str, token_str, price
            );
        }
        _ => {}
    }
}

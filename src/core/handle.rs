use std::sync::Arc;
use tracing::{info, warn};
use polymarket_client_sdk::clob::types::Side as PolySide;
use chrono::Utc;
use uuid::Uuid;
use crate::ui::{self, TradeHistoryEntry, TradeType, OpenOrder};
use super::strat::{RealEngine, StreamType};

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
    let is_up = asset_id == &*engine.up_token;

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
    let token_str = if is_up { "UP" } else { "DOWN" };

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

    // Если есть taker_order_id - запускаем фоновую задачу для освобождения виртуальных лимитов
    if let Some(order_id) = taker_order_id {
        let engine_clone = Arc::clone(engine);
        tokio::spawn(async move {
            handle_taker_fill_async(engine_clone, order_id, size).await;
        });
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
    let is_up = asset_id == &*engine.up_token;

    match msg_type.as_deref() {
        Some("PLACEMENT") => {
            engine.add_order_id(order_id.clone());

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
        }
        Some("UPDATE") => {
            if let Some(size) = size_matched {
                let mut orders_info = engine.active_orders_info.lock().unwrap();

                if let Some((_order_price, _is_up, _original_size, accumulated_filled)) = orders_info.get_mut(&order_id) {
                    let previous_filled = *accumulated_filled;
                    *accumulated_filled += size;

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

                            // Обновляем active_orders и виртуальный лимит
                            handle_order_fully_filled(engine, &order_id, final_is_up, current_original_size);
                        } else {
                            drop(orders_info);
                        }

                        if size_for_portfolio > 0.0 {
                            // Проверяем тип ордера для освобождения виртуальных резервов
                            let stream_type = {
                                let orders = engine.active_orders.lock().unwrap();
                                orders.get(&order_id).map(|o| o.stream_type)
                            };

                            // Если это ExpensiveSide - освобождаем виртуальное закрытие перекоса
                            if let Some(StreamType::ExpensiveSide) = stream_type {
                                let mut limit = engine.virtual_limit.lock().unwrap();

                                let release_skew = size_for_portfolio.min(limit.used_skew_close);
                                limit.used_skew_close = (limit.used_skew_close - release_skew).max(0.0);

                                let release_shares = size_for_portfolio.min(limit.used_shares);
                                limit.used_shares = (limit.used_shares - release_shares).max(0.0);

                                info!("📊 ExpensiveSide partial fill | Освобождено skew: {:.1} | Освобождено shares: {:.1}",
                                    release_skew, release_shares);
                            }

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

            if let Some((order_price, is_up, original_size, accumulated)) = order_info {
                ui::remove_our_bid_price(&engine.ui_state, is_up, order_price);

                // Освобождаем виртуальный лимит для неисполненной части
                let unfilled = original_size - accumulated;
                handle_order_cancelled(engine, &order_id, unfilled);
            }

            ui::remove_open_order(&engine.ui_state, &order_id);

            warn!("❌ MAKER CANCELLED: {} {} @ {:.3}", side_str, token_str, price);
        }
        _ => {}
    }
}

/// Обработка полного заполнения ордера
fn handle_order_fully_filled(engine: &Arc<RealEngine>, order_id: &str, is_up: bool, size: f64) {
    let mut active_orders = engine.active_orders.lock().unwrap();

    if let Some(order) = active_orders.remove(order_id) {
        // Если это ExpensiveSide ордер - освобождаем виртуальное резервирование skew и used_shares
        if matches!(order.stream_type, StreamType::ExpensiveSide) {
            let mut limit = engine.virtual_limit.lock().unwrap();

            // Освобождаем виртуальное закрытие перекоса
            let release_skew = size.min(limit.used_skew_close);
            limit.used_skew_close = (limit.used_skew_close - release_skew).max(0.0);

            // Освобождаем виртуальный лимит cheap side (фактически закрыли перекос)
            let release_shares = size.min(limit.used_shares);
            limit.used_shares = (limit.used_shares - release_shares).max(0.0);

            info!("📊 ExpensiveSide fill | Освобождено skew: {:.1} | Освобождено shares: {:.1}",
                release_skew, release_shares);
        }

        info!("✅ Ордер {} полностью исполнен: {} @ {:.2}",
            order_id, if is_up { "UP" } else { "DOWN" }, order.price);
    }
}

/// Обработка отмены ордера - освобождаем виртуальный лимит
fn handle_order_cancelled(engine: &Arc<RealEngine>, order_id: &str, unfilled_size: f64) {
    let mut active_orders = engine.active_orders.lock().unwrap();

    if let Some(order) = active_orders.remove(order_id) {
        if unfilled_size > 0.0 {
            let mut limit = engine.virtual_limit.lock().unwrap();

            if matches!(order.stream_type, StreamType::CheapSide) {
                // Освобождаем виртуальный лимит для CheapSide ордеров
                let release = unfilled_size.min(limit.used_shares);
                limit.used_shares = (limit.used_shares - release).max(0.0);
                info!("📊 CheapSide cancelled | Освобождено из лимита: {:.1} | Осталось: {:.1}",
                    release, limit.used_shares);
            } else if matches!(order.stream_type, StreamType::ExpensiveSide) {
                // Освобождаем виртуальное закрытие перекоса для ExpensiveSide ордеров
                let release = unfilled_size.min(limit.used_skew_close);
                limit.used_skew_close = (limit.used_skew_close - release).max(0.0);
                info!("📊 ExpensiveSide cancelled | Освобождено skew: {:.1} | Осталось: {:.1}",
                    release, limit.used_skew_close);
            }
        }
    }
}

/// Асинхронная обработка TAKER fill - ждет появления order_id в active_orders
/// Это нужно потому что WebSocket событие может прийти раньше чем API вернет order_id
async fn handle_taker_fill_async(engine: Arc<RealEngine>, order_id: String, size: f64) {
    use tokio::time::{sleep, Duration};

    info!("🔍 TAKER: Ищем order_id {} в active_orders...", order_id);

    // Пытаемся найти ордер в течение 10 секунд (с интервалом 5ms)
    for attempt in 1..=2000 {
        // Проверяем наличие ордера в active_orders
        let stream_type_opt = {
            let orders = engine.active_orders.lock().unwrap();
            orders.get(&order_id).map(|o| o.stream_type)
        };

        if let Some(stream_type) = stream_type_opt {
            // Нашли ордер! Освобождаем виртуальные лимиты
            info!("✅ TAKER: Найден order_id {} (попытка {})", order_id, attempt);

            let mut limit = engine.virtual_limit.lock().unwrap();

            if matches!(stream_type, StreamType::CheapSide) {
                // CheapSide TAKER fill - освобождаем used_shares
                let release = size.min(limit.used_shares);
                limit.used_shares = (limit.used_shares - release).max(0.0);
                info!("📊 TAKER CheapSide | Освобождено shares: {:.1} | Осталось: {:.1}",
                    release, limit.used_shares);
            } else if matches!(stream_type, StreamType::ExpensiveSide) {
                // ExpensiveSide TAKER fill - освобождаем used_skew_close и used_shares
                let release_skew = size.min(limit.used_skew_close);
                limit.used_skew_close = (limit.used_skew_close - release_skew).max(0.0);

                let release_shares = size.min(limit.used_shares);
                limit.used_shares = (limit.used_shares - release_shares).max(0.0);

                info!("📊 TAKER ExpensiveSide | Освобождено skew: {:.1} | Освобождено shares: {:.1}",
                    release_skew, release_shares);
            }

            drop(limit);

            // Удаляем ордер из active_orders
            let mut orders = engine.active_orders.lock().unwrap();
            orders.remove(&order_id);

            return; // Выходим из цикла
        }

        // Ордер еще не добавлен - ждем 5ms и пробуем снова
        if attempt % 200 == 0 {
            info!("🔍 TAKER: Все еще ищем order_id {} (попытка {}/2000)...", order_id, attempt);
        }
        sleep(Duration::from_millis(5)).await;
    }

    // Timeout - не нашли ордер за 10 секунд
    warn!("⚠️ TAKER: Не найден order_id {} в active_orders за 10 секунд (timeout)", order_id);
}

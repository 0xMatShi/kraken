use std::sync::Arc;
use tracing::{info, warn, error};
use polymarket_client_sdk::clob::types::{OrderType, Side as PolySide};
use polymarket_client_sdk::types::Decimal;
use crate::models::MarketPrices;
use super::strat::{RealEngine, HedgeState};

/// Расчет количества акций для хеджа по формуле:
/// Количество = (ПотраченоВсего - АкцийВНаличии) / (1 - ЦенаПокупки)
pub fn calculate_hedge_size(total_spent: f64, shares_on_hand: f64, price: f64) -> f64 {
    if price >= 1.0 {
        warn!("⚠️ Невозможно рассчитать хедж: цена >= 1.0 ({:.2})", price);
        return 0.0;
    }
    let size = (total_spent - shares_on_hand) / (1.0 - price);
    size.max(0.0)
}

/// Проверяем нужен ли хедж и запускаем его размещение
pub fn check_and_start_hedge(engine: &Arc<RealEngine>, prices: &MarketPrices) {
    // Пропускаем только если хедж УЖЕ РАЗМЕЩЕН (есть order_id)
    let hedge_order_placed = {
        let hedge = engine.hedge_state.lock().unwrap();
        hedge.as_ref().and_then(|h| h.order_id.as_ref()).is_some()
    };

    if hedge_order_placed {
        return;
    }

    let (up_shares, down_shares, total_spent) = {
        let port = engine.portfolio.lock().unwrap();
        (port.up_shares, port.down_shares, port.up_spent + port.down_spent)
    };

    let skew = up_shares - down_shares;

    // ПОРОГ АКТИВАЦИИ: перекос > 50 акций
    if skew.abs() <= 50.0 {
        let hedging_was_active = *engine.hedging_active.lock().unwrap();
        if hedging_was_active {
            info!("✅ Перекос устранен естественным образом (Skew={:.1})", skew);
            *engine.hedging_active.lock().unwrap() = false;
            *engine.hedge_state.lock().unwrap() = None;
            info!("🔓 Потоки разблокированы");
        }
        return;
    }

    // КРИТИЧНО: Атомарно проверяем и устанавливаем флаг
    {
        let mut active = engine.hedging_active.lock().unwrap();
        if *active {
            return;
        }
        *active = true;
    };

    info!("⚖️ ОБНАРУЖЕН ПЕРЕКОС: UP={:.1} DOWN={:.1} | Skew={:.1}",
        up_shares, down_shares, skew);

    // Проверяем, есть ли активные потоки
    if engine.has_active_threads() {
        info!("⏳ Есть активные потоки - ждем их возврата в Idle перед хеджем");
        info!("🔒 Новые потоки заблокированы до завершения хеджа");
        return;
    }

    // Определяем недостающую сторону
    let (is_up_side, shares_on_hand, best_bid) = if skew > 0.0 {
        (false, down_shares, prices.down_bid)
    } else {
        (true, up_shares, prices.up_bid)
    };

    let target_size = calculate_hedge_size(total_spent, shares_on_hand, best_bid);

    if target_size < 5.0 {
        warn!("⚠️ Размер хеджа < 5.0 ({:.2}), пропускаем", target_size);
        *engine.hedging_active.lock().unwrap() = false;
        return;
    }

    info!("🎯 ЗАПУСК ХЕДЖА | Сторона: {} | Best_bid: {:.2} | Целевой размер: {:.2}",
        if is_up_side { "UP" } else { "DOWN" }, best_bid, target_size);

    *engine.hedge_state.lock().unwrap() = Some(HedgeState {
        order_id: None,
        is_up_side,
        current_price: best_bid,
        target_size,
        filled_size: 0.0,
        pending_repricing: None,
    });

    place_hedge_order(engine, is_up_side, best_bid, target_size);
}

/// Размещаем лимитку хеджа
pub fn place_hedge_order(engine: &Arc<RealEngine>, is_up_side: bool, price: f64, size: f64) {
    let token_id = if is_up_side {
        Arc::clone(&engine.up_token)
    } else {
        Arc::clone(&engine.down_token)
    };

    let client = engine.client.clone();
    let signer = engine.signer.clone();
    let engine = Arc::clone(engine);

    info!("📝 ХЕДЖ | Размещаем лимитку: {} @ {:.2} | Size: {:.2}",
        if is_up_side { "UP" } else { "DOWN" }, price, size);

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
                    info!("✅ ХЕДЖ | Лимитка размещена: order_id={}", response.order_id);

                    let mut hedge = engine.hedge_state.lock().unwrap();
                    if let Some(ref mut state) = *hedge {
                        state.order_id = Some(response.order_id);
                    }
                }
            },
            Err(e) => {
                error!("❌ ХЕДЖ | Ошибка размещения: {}", e);
                *engine.hedge_state.lock().unwrap() = None;
            },
        }
    });
}

/// Проверяем нужно ли перевыставить хедж при изменении best_bid
pub fn check_hedge_repricing(engine: &Arc<RealEngine>, prices: &MarketPrices) {
    let hedge = engine.hedge_state.lock().unwrap();

    if let Some(ref state) = *hedge {
        if let Some(ref order_id) = state.order_id {
            let new_best_bid = if state.is_up_side {
                prices.up_bid
            } else {
                prices.down_bid
            };

            let new_best_bid = RealEngine::round_price(new_best_bid);
            let price_diff = new_best_bid - state.current_price;

            if price_diff >= 0.01 {
                let (total_spent, shares_on_hand) = {
                    let port = engine.portfolio.lock().unwrap();
                    let total = port.up_spent + port.down_spent;
                    let shares = if state.is_up_side {
                        port.up_shares
                    } else {
                        port.down_shares
                    };
                    (total, shares)
                };

                let new_target_size = calculate_hedge_size(
                    total_spent,
                    shares_on_hand,
                    new_best_bid
                );

                let remaining_size = new_target_size - state.filled_size;

                if remaining_size < 5.0 {
                    warn!("⚠️ ХЕДЖ | Оставшийся размер < 5.0 ({:.2}), пропускаем перевыставление", remaining_size);
                    return;
                }

                info!("🔄 ХЕДЖ | Best_bid ПОВЫСИЛСЯ: {:.2} → {:.2} (+{:.2})",
                    state.current_price, new_best_bid, price_diff);
                info!("   Новый целевой размер: {:.2} | Осталось разместить: {:.2}",
                    new_target_size, remaining_size);

                let order_id_to_cancel = order_id.clone();
                drop(hedge);

                let client = engine.client.clone();
                let engine = Arc::clone(engine);

                tokio::spawn(async move {
                    info!("🚫 ХЕДЖ | Отменяем ордер: {}", order_id_to_cancel);

                    let cancel_success = match client.cancel_order(&order_id_to_cancel).await {
                        Ok(result) => {
                            if !result.canceled.is_empty() {
                                info!("✅ ХЕДЖ | Ордер отменён");
                                true
                            } else {
                                warn!("⚠️ ХЕДЖ | Ордер не был отменён");
                                false
                            }
                        },
                        Err(e) => {
                            error!("❌ ХЕДЖ | Ошибка отмены: {}", e);
                            false
                        }
                    };

                    if cancel_success {
                        {
                            let mut hedge = engine.hedge_state.lock().unwrap();
                            if let Some(ref mut state) = *hedge {
                                info!("⏳ ХЕДЖ | Ордер отменен API, ждем WebSocket CANCELLATION для перевыставления @ {:.2}", new_best_bid);
                                state.order_id = None;
                                state.current_price = new_best_bid;
                                state.target_size = new_target_size;
                                state.pending_repricing = Some((new_best_bid, remaining_size));
                            }
                        }
                    }
                });
            }
        }
    }
}

/// Обновляем filled_size хеджа при UPDATE события
pub fn update_hedge_filled(engine: &RealEngine, order_id: &str, size: f64) {
    let mut hedge = engine.hedge_state.lock().unwrap();

    if let Some(ref mut state) = *hedge {
        if let Some(ref hedge_order_id) = state.order_id {
            if order_id == hedge_order_id {
                state.filled_size += size;
                info!("📊 ХЕДЖ | Частичное исполнение: {:.2} | Всего: {:.2}/{:.2}",
                    size, state.filled_size, state.target_size);
            }
        }
    }
}

/// Завершаем хедж когда ордер полностью исполнен
pub fn complete_hedge(engine: &RealEngine, order_id: &str) {
    let mut hedge = engine.hedge_state.lock().unwrap();

    if let Some(ref state) = *hedge {
        if let Some(ref hedge_order_id) = state.order_id {
            if order_id == hedge_order_id {
                info!("✅ ХЕДЖ ЗАВЕРШЕН | Filled: {:.2}/{:.2}",
                    state.filled_size, state.target_size);

                *hedge = None;
                drop(hedge);

                *engine.hedging_active.lock().unwrap() = false;
                info!("🔓 Потоки разблокированы");
            }
        }
    }
}

/// Отменяем хедж если ордер был отменен
pub fn cancel_hedge(engine: &Arc<RealEngine>, order_id: &str) {
    let mut hedge = engine.hedge_state.lock().unwrap();

    if let Some(ref state) = *hedge {
        if let Some(ref hedge_order_id) = state.order_id {
            if order_id == hedge_order_id {
                warn!("⚠️ ХЕДЖ ОТМЕНЕН (WebSocket CANCELLATION)");

                let pending = state.pending_repricing;
                let is_up_side = state.is_up_side;

                if let Some((new_price, new_size)) = pending {
                    info!("✅ WebSocket CANCELLATION подтвержден, размещаем новый хедж @ {:.2} | Size: {:.2}",
                        new_price, new_size);

                    if let Some(ref mut s) = *hedge {
                        s.order_id = None;
                        s.pending_repricing = None;
                    }
                    drop(hedge);

                    place_hedge_order(engine, is_up_side, new_price, new_size);
                } else {
                    *hedge = None;
                    drop(hedge);

                    *engine.hedging_active.lock().unwrap() = false;
                    info!("🔓 Потоки разблокированы");
                }
            }
        }
    }
}

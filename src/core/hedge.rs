use std::sync::Arc;
use tracing::{info, warn, error};
use polymarket_client_sdk::clob::types::{OrderType, Side as PolySide};
use polymarket_client_sdk::types::Decimal;
use crate::models::MarketPrices;
use super::strat::{RealEngine, HedgeState};

/// Расчет количества акций для хеджа с ограничением по бюджету (75% от прибыли)
///
/// Логика:
/// 1. Определяем выигрывающую сторону (best_bid > 0.5)
/// 2. Прибыль = shares_on_winning_side - total_spent
/// 3. Максимальный бюджет хеджа = прибыль * 0.75
/// 4. Рассчитываем полный размер хеджа: (total_spent - shares_on_hand) / (1 - price)
/// 5. Если стоимость хеджа > max_budget → ограничиваем размер
pub fn calculate_hedge_size(
    total_spent: f64,
    shares_on_hand: f64,
    price: f64,
    up_shares: f64,
    down_shares: f64,
    up_bid: f64,
    down_bid: f64
) -> f64 {
    if price >= 1.0 {
        warn!("⚠️ Невозможно рассчитать хедж: цена >= 1.0 ({:.2})", price);
        return 0.0;
    }

    // Определяем выигрывающую сторону
    let (winning_side_shares, winning_side_name) = if up_bid > 0.5 {
        (up_shares, "UP")
    } else if down_bid > 0.5 {
        (down_shares, "DOWN")
    } else {
        // Нет явного победителя → используем старую логику без ограничений
        let size = (total_spent - shares_on_hand) / (1.0 - price);
        return size.max(0.0);
    };

    // Рассчитываем полный размер хеджа (без ограничений)
    let full_hedge_size = (total_spent - shares_on_hand) / (1.0 - price);
    let full_hedge_size = full_hedge_size.max(0.0);

    // Рассчитываем текущую прибыль
    let current_profit = winning_side_shares - total_spent;

    if current_profit <= 0.0 {
        // Прибыли нет → хеджимся на 50% от полного размера
        let half_size = full_hedge_size * 0.5;
        let half_cost = half_size * price;
        warn!("⚠️ Текущая прибыль <= 0 ({:.2}$)", current_profit);
        info!("   Хеджимся на 50% от полного размера: {:.2} акций за {:.2}$",
            half_size, half_cost);
        return half_size;
    }

    // Максимальный бюджет = 75% от прибыли
    let max_budget = current_profit * 0.75;

    info!("💰 Прибыль на стороне {}: {:.2}$ | Макс бюджет хеджа (75%): {:.2}$",
        winning_side_name, current_profit, max_budget);

    // Стоимость полного хеджа
    let full_hedge_cost = full_hedge_size * price;

    if full_hedge_cost <= max_budget {
        // Полный хедж укладывается в бюджет → используем его
        info!("✅ Полный хедж ({:.2} акций за {:.2}$) укладывается в бюджет",
            full_hedge_size, full_hedge_cost);
        return full_hedge_size;
    }

    // Полный хедж превышает бюджет → ограничиваем размер
    let limited_size = max_budget / price;
    let budget_usage_percent = (full_hedge_cost / current_profit) * 100.0;

    info!("⚠️ Полный хедж ({:.2} акций за {:.2}$) превышает бюджет",
        full_hedge_size, full_hedge_cost);
    info!("   Это {:.1}% от прибыли (лимит: 75%)", budget_usage_percent);
    info!("   Ограничиваем размер до {:.2} акций за {:.2}$",
        limited_size, max_budget);

    limited_size
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
    let total_shares = up_shares + down_shares;

    // ПОРОГ АКТИВАЦИИ: динамический расчет
    // Минимум: 15 акций
    // Максимум: 3% от total_spent
    let threshold = (total_shares * 0.03).max(30.0);

    if skew.abs() <= threshold {
        let hedging_was_active = *engine.hedging_active.lock().unwrap();
        if hedging_was_active {
            info!("✅ Перекос устранен естественным образом (Skew={:.1}, Threshold={:.1})", skew, threshold);
            *engine.hedging_active.lock().unwrap() = false;
            *engine.hedge_state.lock().unwrap() = None;
            info!("🔓 Потоки разблокированы");
        }
        return;
    }

    // Проверяем, активен ли уже хедж
    {
        let active = engine.hedging_active.lock().unwrap();
        if *active {
            return;
        }
    }

    info!("⚖️ ОБНАРУЖЕН ПЕРЕКОС: UP={:.1} DOWN={:.1} | Skew={:.1} (Threshold={:.1})",
        up_shares, down_shares, skew, threshold);

    // Проверяем, есть ли активные потоки
    if engine.has_active_threads() {
        info!("⏳ Есть активные потоки - ждем их возврата в Idle перед хеджем");
        info!("🔒 Новые потоки заблокированы до завершения хеджа");
        return;
    }

    // КРИТИЧНО: Устанавливаем флаг ТОЛЬКО если нет активных потоков
    {
        let mut active = engine.hedging_active.lock().unwrap();
        if *active {
            return; // Double-check на случай race condition
        }
        *active = true;
    }

    // Определяем недостающую сторону
    let (is_up_side, shares_on_hand, best_bid) = if skew > 0.0 {
        (false, down_shares, prices.down_bid)
    } else {
        (true, up_shares, prices.up_bid)
    };

    let target_size = calculate_hedge_size(
        total_spent,
        shares_on_hand,
        best_bid,
        up_shares,
        down_shares,
        prices.up_bid,
        prices.down_bid
    );

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
        api_cancel_confirmed: false,
        websocket_cancel_confirmed: false,
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
    let mut hedge = engine.hedge_state.lock().unwrap();

    if let Some(ref state) = *hedge {
        if let Some(ref order_id) = state.order_id {
            let new_best_bid = if state.is_up_side {
                prices.up_bid
            } else {
                prices.down_bid
            };

            let new_best_bid = RealEngine::round_price(new_best_bid);

            // Если уже есть pending_repricing, проверяем нужно ли обновить цену
            if let Some((pending_price, _pending_size)) = state.pending_repricing {
                let price_improvement = new_best_bid - pending_price;
                if price_improvement >= 0.01 {
                    // Пересчитываем размер для новой цены
                    let (total_spent, shares_on_hand, up_shares, down_shares) = {
                        let port = engine.portfolio.lock().unwrap();
                        let total = port.up_spent + port.down_spent;
                        let shares = if state.is_up_side {
                            port.up_shares
                        } else {
                            port.down_shares
                        };
                        (total, shares, port.up_shares, port.down_shares)
                    };

                    let new_target_size = calculate_hedge_size(
                        total_spent,
                        shares_on_hand,
                        new_best_bid,
                        up_shares,
                        down_shares,
                        prices.up_bid,
                        prices.down_bid
                    );
                    let new_remaining_size = new_target_size - state.filled_size;

                    if new_remaining_size >= 5.0 {
                        // Обновляем pending_repricing с новой ценой и размером
                        if let Some(ref mut hedge_state) = *hedge {
                            hedge_state.pending_repricing = Some((new_best_bid, new_remaining_size));
                            info!("📈 ХЕДЖ | Обновляем pending_repricing: {:.2} → {:.2} | Size: {:.2}",
                                pending_price, new_best_bid, new_remaining_size);
                        }
                    }
                }
                return; // Отмена уже запущена, ждём подтверждений
            }

            let price_diff = new_best_bid - state.current_price;

            if price_diff >= 0.01 {
                let (total_spent, shares_on_hand, up_shares, down_shares) = {
                    let port = engine.portfolio.lock().unwrap();
                    let total = port.up_spent + port.down_spent;
                    let shares = if state.is_up_side {
                        port.up_shares
                    } else {
                        port.down_shares
                    };
                    (total, shares, port.up_shares, port.down_shares)
                };

                let new_target_size = calculate_hedge_size(
                    total_spent,
                    shares_on_hand,
                    new_best_bid,
                    up_shares,
                    down_shares,
                    prices.up_bid,
                    prices.down_bid
                );

                let remaining_size = new_target_size - state.filled_size;

                if remaining_size < 5.0 {
                    return;
                }

                info!("🔄 ХЕДЖ | Best_bid ПОВЫСИЛСЯ: {:.2} → {:.2} (+{:.2})",
                    state.current_price, new_best_bid, price_diff);
                info!("   Новый целевой размер: {:.2} | Осталось разместить: {:.2}",
                    new_target_size, remaining_size);

                let order_id_to_cancel = order_id.clone();
                let is_up_side = state.is_up_side;
                let filled_size = state.filled_size;

                // КЛЮЧЕВОЕ ИЗМЕНЕНИЕ: СРАЗУ обновляем состояние, сбрасываем order_id
                // и устанавливаем pending_repricing. Это предотвращает повторные попытки отмены.
                *hedge = Some(HedgeState {
                    order_id: Some(order_id_to_cancel.clone()),  // Сохраняем для WebSocket matching
                    is_up_side,
                    current_price: new_best_bid,  // Уже обновляем цену
                    target_size: new_target_size,
                    filled_size,
                    pending_repricing: Some((new_best_bid, remaining_size)),
                    api_cancel_confirmed: false,        // Ждём подтверждение от API
                    websocket_cancel_confirmed: false,  // Ждём подтверждение от WebSocket
                });

                info!("🔑 ХЕДЖ | Состояние обновлено: ожидаем 2 ключа (API + WebSocket) для подтверждения отмены");

                drop(hedge);

                let client = engine.client.clone();
                let engine_clone = Arc::clone(engine);

                // Асинхронно запрашиваем отмену через API
                tokio::spawn(async move {
                    info!("🚫 ХЕДЖ | Отправляем запрос на отмену ордера: {}", order_id_to_cancel);

                    let cancel_success = match client.cancel_order(&order_id_to_cancel).await {
                        Ok(result) => {
                            if !result.canceled.is_empty() {
                                info!("✅ ХЕДЖ | API подтвердил отмену ордера: {}", order_id_to_cancel);
                                true
                            } else {
                                warn!("⚠️ ХЕДЖ | API НЕ подтвердил отмену: {}", order_id_to_cancel);
                                false
                            }
                        },
                        Err(e) => {
                            error!("❌ ХЕДЖ | Ошибка отмены: {}", e);
                            false
                        }
                    };

                    if cancel_success {
                        // КЛЮЧ 1: API подтвердил отмену
                        let should_place_new_order = {
                            let mut hedge = engine_clone.hedge_state.lock().unwrap();
                            if let Some(ref mut state) = *hedge {
                                state.api_cancel_confirmed = true;
                                info!("🔑 ХЕДЖ | КЛЮЧ 1/2 ПОВЕРНУТ (API)");

                                // Проверяем: если оба ключа повернуты → размещаем новый ордер
                                if state.websocket_cancel_confirmed && state.pending_repricing.is_some() {
                                    info!("🔓 ХЕДЖ | ОБА КЛЮЧА ПОВЕРНУТЫ! Размещаем новый ордер");
                                    true
                                } else {
                                    info!("⏳ ХЕДЖ | Ждём КЛЮЧ 2/2 (WebSocket CANCELLATION)");
                                    false
                                }
                            } else {
                                false
                            }
                        };

                        if should_place_new_order {
                            let (is_up_side, new_price, new_size) = {
                                let hedge = engine_clone.hedge_state.lock().unwrap();
                                if let Some(ref state) = *hedge {
                                    if let Some((price, size)) = state.pending_repricing {
                                        (state.is_up_side, price, size)
                                    } else {
                                        return;
                                    }
                                } else {
                                    return;
                                }
                            };

                            place_hedge_order(&engine_clone, is_up_side, new_price, new_size);
                        }
                    } else {
                        // API вернул false (ордер не найден) → значит другой тик уже отменил или ордер заполнен
                        // Ничего не делаем, предыдущий тик уже обработал ситуацию
                        warn!("⚠️ ХЕДЖ | API отмена не удалась (ордер не найден) - другой тик уже обработал");
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
                let api_already_confirmed = state.api_cancel_confirmed;

                // КЛЮЧ 2: WebSocket подтвердил отмену
                // Проверяем: если API уже подтвердил (Ключ 1) → размещаем новый ордер
                let should_place_new_order = if let Some((_new_price, _new_size)) = pending {
                    if api_already_confirmed {
                        info!("🔑 ХЕДЖ | КЛЮЧ 2/2 ПОВЕРНУТ (WebSocket)");
                        info!("🔓 ХЕДЖ | ОБА КЛЮЧА ПОВЕРНУТЫ! Размещаем новый ордер");
                        true
                    } else {
                        info!("🔑 ХЕДЖ | КЛЮЧ 2/2 ПОВЕРНУТ (WebSocket)");
                        info!("⏳ ХЕДЖ | Ждём КЛЮЧ 1/2 (API confirmation)");
                        false
                    }
                } else {
                    // Нет pending_repricing → это обычная отмена (не перевыставление)
                    info!("⚠️ ХЕДЖ | Обычная отмена (не перевыставление), завершаем хедж");
                    *hedge = None;
                    drop(hedge);
                    *engine.hedging_active.lock().unwrap() = false;
                    info!("🔓 Потоки разблокированы");
                    return;
                };

                if should_place_new_order {
                    // Оба ключа повернуты → сбрасываем pending и размещаем новый ордер
                    let (new_price, new_size) = pending.unwrap();

                    if let Some(ref mut s) = *hedge {
                        s.order_id = None;
                        s.pending_repricing = None;
                        s.api_cancel_confirmed = false;
                        s.websocket_cancel_confirmed = false;
                    }
                    drop(hedge);

                    info!("✅ ХЕДЖ | Размещаем новый ордер @ {:.2} | Size: {:.2}",
                        new_price, new_size);
                    place_hedge_order(engine, is_up_side, new_price, new_size);
                } else {
                    // WebSocket подтвердил, но API ещё нет → обновляем флаг и ждём
                    if let Some(ref mut s) = *hedge {
                        s.websocket_cancel_confirmed = true;  // Устанавливаем флаг
                        s.order_id = None;  // Сбрасываем order_id чтобы не обрабатывать CANCELLATION повторно
                    }
                }
            }
        }
    }
}

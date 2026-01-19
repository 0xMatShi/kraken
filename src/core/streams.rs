use std::sync::Arc;
use tracing::{info, warn};
use polymarket_client_sdk::clob::types::{OrderType, Side as PolySide};
use polymarket_client_sdk::types::Decimal;
use chrono::Utc;
use crate::models::Side;
use super::strat::RealEngine;

/// Новая логика размещения ордеров
/// Размещает ордера на обе стороны каждые 0.5 секунды
pub fn process_order_placement(engine: &Arc<RealEngine>, up_bid: f64, down_bid: f64) {
    let port = engine.portfolio.lock().unwrap();
    let up_shares = port.up_shares;
    let down_shares = port.down_shares;
    let up_avg = port.up_avg();
    let down_avg = port.down_avg();
    let total_avg = port.total_avg();

    // Рассчитываем перекос в процентах и в абсолютных значениях
    let total_shares = up_shares + down_shares;
    let skew_abs = (up_shares - down_shares).abs();
    let skew_percent = if total_shares > 0.0 {
        (skew_abs / total_shares) * 100.0
    } else {
        0.0
    };

    drop(port);

    info!("📊 Portfolio State: UP {:.1} @ {:.3} | DOWN {:.1} @ {:.3} | Total Avg: {:.3} | Skew: {:.1}% ({:.1} акций)",
        up_shares, up_avg, down_shares, down_avg, total_avg, skew_percent, skew_abs);

    // Если баланс 0/0 - размещаем на обе стороны одновременно
    if up_shares == 0.0 && down_shares == 0.0 {
        info!("🎯 Начальная фаза (0/0): размещаем на обе стороны");
        place_order_on_side(engine, Side::Up, up_bid);
        place_order_on_side(engine, Side::Down, down_bid);
        return; // Возвращаемся, так как это специальный случай старта
    }

    // ЗАКОММЕНТИРОВАНО: Старая логика закрытия большого перекоса
    // if skew_percent > 2.0 && skew_abs > 100.0 {
    //     // Определяем сторону для закрытия перекоса
    //     let (buy_side, buy_price, avg_side_with_more) = if up_shares < down_shares {
    //         // DOWN больше, покупаем UP
    //         (Side::Up, up_bid, down_avg)
    //     } else {
    //         // UP больше, покупаем DOWN
    //         (Side::Down, down_bid, up_avg)
    //     };
    //
    //     // Рассчитываем ratio = (1 - avg_side_with_more) / buy_price
    //     let ratio = if buy_price > 0.0 && avg_side_with_more > 0.0 {
    //         (1.0 - avg_side_with_more) / buy_price
    //     } else {
    //         1.0 // Если avg = 0 или price = 0, используем ratio = 1.0
    //     };
    //
    //     // Рассчитываем размер для закрытия: skew_size = ratio * skew
    //     let skew_size = ratio * skew_abs;
    //
    //     // Рассчитываем количество ордеров: floor(skew_size / size)
    //     let order_size = engine.config.size;
    //     let mut num_orders = ((skew_size / order_size) / 2.0).floor() as usize;
    //
    //     // Проверяем max_balance для всех ордеров сразу
    //     let total_cost = buy_price * order_size * num_orders as f64;
    //     {
    //         let port = engine.portfolio.lock().unwrap();
    //         let total_spent = port.up_spent + port.down_spent;
    //         if total_spent + total_cost > engine.config.max_balance {
    //             // Уменьшаем количество ордеров, чтобы не превысить max_balance
    //             let available_budget = engine.config.max_balance - total_spent;
    //             let max_orders = (available_budget / (buy_price * order_size)).floor() as usize;
    //             num_orders = num_orders.min(max_orders);
    //             info!("⚠️ Max balance ограничение: уменьшаем количество ордеров до {}", num_orders);
    //         }
    //     }
    //
    //     if num_orders == 0 {
    //         info!("⚠️ Недостаточно баланса для закрытия перекоса");
    //         return;
    //     }
    //
    //     info!("⚠️ Перекос {:.1}% ({:.1} акций) | Ratio: {:.3} | Skew Size: {:.1} | Размещаем {} ордеров по {:.1}",
    //         skew_percent, skew_abs, ratio, skew_size, num_orders, order_size);
    //
    //     // Размещаем рассчитанное количество ордеров
    //     for i in 0..num_orders {
    //         info!("📝 Ордер {}/{} для закрытия перекоса: {:?} @ {:.2}",
    //             i + 1, num_orders, buy_side, buy_price);
    //         place_order_on_side(engine, buy_side, buy_price);
    //     }
    //
    //     return;
    // }

    // Определяем cheap и expensive стороны по новым правилам:
    // cheap: best_bid < 0.5
    // expensive: best_bid >= 0.5
    let up_is_cheap = up_bid < 0.5;
    let down_is_cheap = down_bid < 0.5;

    info!("📊 Sides: UP {} | DOWN {}",
        if up_is_cheap { "CHEAP" } else { "EXPENSIVE" },
        if down_is_cheap { "CHEAP" } else { "EXPENSIVE" });

    // ДОПОЛНИТЕЛЬНОЕ ПРАВИЛО: Если перекос > size И сторона с меньшим количеством имеет bid < avg - закрыть перекос
    // Логика: если можно купить дешевле текущего avg И это закроет перекос - делаем это
    let order_size = engine.config.size;
    if skew_abs > order_size {
        // Определяем сторону с меньшим количеством акций и проверяем, дешевая ли она (bid < avg)
        let skew_against_cheap = if up_shares < down_shares {
            // UP меньше - проверяем, дешевая ли UP (bid < avg)
            up_avg > 0.0 && up_bid < up_avg
        } else if down_shares < up_shares {
            // DOWN меньше - проверяем, дешевая ли DOWN (bid < avg)
            down_avg > 0.0 && down_bid < down_avg
        } else {
            false // Нет перекоса
        };

        if skew_against_cheap {
            // Перекос НЕ В ПОЛЬЗУ дешёвой стороны (bid < avg) - закрываем его
            let (buy_side, buy_price, avg_side_with_more) = if up_shares < down_shares {
                // UP меньше (и дешевая), покупаем UP
                (Side::Up, up_bid, down_avg)
            } else {
                // DOWN меньше (и дешевая), покупаем DOWN
                (Side::Down, down_bid, up_avg)
            };

            // Рассчитываем ratio = (1 - avg_side_with_more) / buy_price
            let ratio = if buy_price > 0.0 && avg_side_with_more > 0.0 {
                (1.0 - avg_side_with_more) / buy_price
            } else {
                1.0
            };

            let skew_size = ratio * skew_abs;
            let mut num_orders = ((skew_size / order_size) / 2.0).floor() as usize;

            // Проверяем max_balance
            let total_cost = buy_price * order_size * num_orders as f64;
            {
                let port = engine.portfolio.lock().unwrap();
                let total_spent = port.up_spent + port.down_spent;
                if total_spent + total_cost > engine.config.max_balance {
                    let available_budget = engine.config.max_balance - total_spent;
                    let max_orders = (available_budget / (buy_price * order_size)).floor() as usize;
                    num_orders = num_orders.min(max_orders);
                }
            }

            if num_orders > 0 {
                info!("💎 Перекос {:.1} акций НЕ В ПОЛЬЗУ дешёвой стороны (bid < avg) → Закрываем полностью ({} ордеров)", skew_abs, num_orders);
                for i in 0..num_orders {
                    info!("📝 Ордер {}/{} для закрытия перекоса: {:?} @ {:.2} (avg: {:.3})",
                        i + 1, num_orders, buy_side, buy_price,
                        if buy_side == Side::Up { up_avg } else { down_avg });
                    place_order_on_side(engine, buy_side, buy_price);
                }
                // НЕ возвращаемся, продолжаем дальше для размещения cheap ордеров
            }
        }
    }

    // ЛОГИКА P_max: Закрытие перекоса по максимальной выгодной цене
    // Проверяем перекос и рассчитываем P_max для стороны с меньшим количеством акций
    let mut calculated_p_max: Option<f64> = None;

    if skew_abs > order_size {
        let port = engine.portfolio.lock().unwrap();
        let total_spent = port.up_spent + port.down_spent;
        drop(port);

        // Определяем параметры для расчёта P_max
        let (deficit_side, deficit_bid, s_target, shares_to_buy) = if up_shares < down_shares {
            // UP меньше, цель - выровнять до DOWN
            (Side::Up, up_bid, down_shares, down_shares - up_shares)
        } else if down_shares < up_shares {
            // DOWN меньше, цель - выровнять до UP
            (Side::Down, down_bid, up_shares, up_shares - down_shares)
        } else {
            // Нет перекоса
            (Side::Up, 0.0, 0.0, 0.0)
        };

        if shares_to_buy > 0.0 {
            // P_max = (S_target - total_spent) / shares_to_buy
            let p_max = (s_target - total_spent) / shares_to_buy;
            calculated_p_max = Some(p_max);

            info!("📊 P_max расчёт: S_target={:.1} | total_spent={:.2} | shares_to_buy={:.1} → P_max={:.3}",
                s_target, total_spent, shares_to_buy, p_max);

            // Если best_bid на дефицитной стороне < P_max, закрываем перекос
            if deficit_bid > 0.0 && deficit_bid < p_max {
                // Рассчитываем количество ордеров для покрытия перекоса
                let mut num_orders = ((shares_to_buy / order_size) / 2.0).ceil() as usize;

                // Проверяем max_balance
                let total_cost = deficit_bid * order_size * num_orders as f64;
                {
                    let port = engine.portfolio.lock().unwrap();
                    let total_spent = port.up_spent + port.down_spent;
                    if total_spent + total_cost > engine.config.max_balance {
                        let available_budget = engine.config.max_balance - total_spent;
                        let max_orders = (available_budget / (deficit_bid * order_size)).floor() as usize;
                        num_orders = num_orders.min(max_orders);
                    }
                }

                if num_orders > 0 {
                    info!("💰 P_max условие: best_bid {:.3} < P_max {:.3} → Закрываем перекос ({} ордеров)",
                        deficit_bid, p_max, num_orders);
                    for i in 0..num_orders {
                        info!("📝 Ордер {}/{} P_max закрытие: {:?} @ {:.2}",
                            i + 1, num_orders, deficit_side, deficit_bid);
                        place_order_on_side(engine, deficit_side, deficit_bid);
                    }
                    // НЕ возвращаемся, продолжаем дальше для размещения cheap ордеров
                }
            } else {
                info!("⏸️ P_max условие не выполнено: best_bid {:.3} >= P_max {:.3}", deficit_bid, p_max);
            }
        }
    }

    // ЛОГИКА ЗАКРЫТИЯ УБЫТКА ПО EXPENSIVE СТОРОНЕ ПРИ ПЕРЕКОСЕ
    // N = (total_spent - S_exp) / (1 - P_exp)
    // Применяется ТОЛЬКО если текущий bid > p_max + 0.05
    if let Some(p_max) = calculated_p_max {
        let port = engine.portfolio.lock().unwrap();
        let total_spent = port.up_spent + port.down_spent;
        drop(port);

        // Определяем expensive сторону и проверяем перекос
        let (is_up_expensive, is_down_expensive) = (!up_is_cheap, !down_is_cheap);

        // Проверяем UP expensive в дефиците (срабатывает только при перекосе > 50 акций)
        if is_up_expensive && up_shares < down_shares && up_bid > p_max + 0.03 && skew_abs > 50.0 {
            let s_exp = up_shares;
            let p_exp = up_bid;

            if p_exp < 1.0 && p_exp > 0.0 {
                let margin = 0.01;
                let n = (total_spent * (1.0 + margin) - s_exp) / (1.0 - p_exp * (1.0 + margin));

                if n > 0.0 {
                    info!("📊 Expensive убыток (UP): total_spent={:.2} | S_exp={:.1} | P_exp={:.3} → N={:.1}",
                        total_spent, s_exp, p_exp, n);

                    // Рассчитываем количество ордеров
                    let mut num_orders = ((n / order_size) / 2.0).ceil() as usize;

                    // Проверяем max_balance
                    let total_cost = p_exp * order_size * num_orders as f64;
                    {
                        let port = engine.portfolio.lock().unwrap();
                        let total_spent = port.up_spent + port.down_spent;
                        if total_spent + total_cost > engine.config.max_balance {
                            let available_budget = engine.config.max_balance - total_spent;
                            let max_orders = (available_budget / (p_exp * order_size)).floor() as usize;
                            num_orders = num_orders.min(max_orders);
                        }
                    }

                    if num_orders > 0 {
                        info!("💎 Закрытие убытка expensive (UP): N={:.1} акций → {} ордеров @ {:.2}",
                            n, num_orders, p_exp);
                        for i in 0..num_orders {
                            info!("📝 Ордер {}/{} expensive убыток: UP @ {:.2}",
                                i + 1, num_orders, p_exp);
                            place_order_on_side(engine, Side::Up, up_bid);
                        }
                        // НЕ возвращаемся, продолжаем дальше для размещения cheap ордеров
                    }
                }
            }
        } else if is_up_expensive && up_shares < down_shares {
            info!("⏸️ UP expensive убыток: bid {:.3} <= p_max + 0.03 ({:.3})", up_bid, p_max + 0.03);
        }

        // Проверяем DOWN expensive в дефиците (срабатывает только при перекосе > 50 акций)
        if is_down_expensive && down_shares < up_shares && down_bid > p_max + 0.03 && skew_abs > 50.0 {
            let s_exp = down_shares;
            let p_exp = down_bid;

            if p_exp < 1.0 && p_exp > 0.0 {
                let margin = 0.01;
                let n = (total_spent * (1.0 + margin) - s_exp) / (1.0 - p_exp * (1.0 + margin));

                if n > 0.0 {
                    info!("📊 Expensive убыток (DOWN): total_spent={:.2} | S_exp={:.1} | P_exp={:.3} → N={:.1}",
                        total_spent, s_exp, p_exp, n);

                    // Рассчитываем количество ордеров
                    let mut num_orders = ((n / order_size) / 2.0).ceil() as usize;

                    // Проверяем max_balance
                    let total_cost = p_exp * order_size * num_orders as f64;
                    {
                        let port = engine.portfolio.lock().unwrap();
                        let total_spent = port.up_spent + port.down_spent;
                        if total_spent + total_cost > engine.config.max_balance {
                            let available_budget = engine.config.max_balance - total_spent;
                            let max_orders = (available_budget / (p_exp * order_size)).floor() as usize;
                            num_orders = num_orders.min(max_orders);
                        }
                    }

                    if num_orders > 0 {
                        info!("💎 Закрытие убытка expensive (DOWN): N={:.1} акций → {} ордеров @ {:.2}",
                            n, num_orders, p_exp);
                        for i in 0..num_orders {
                            info!("📝 Ордер {}/{} expensive убыток: DOWN @ {:.2}",
                                i + 1, num_orders, p_exp);
                            place_order_on_side(engine, Side::Down, down_bid);
                        }
                        // НЕ возвращаемся, продолжаем дальше для размещения cheap ордеров
                    }
                }
            }
        } else if is_down_expensive && down_shares < up_shares {
            info!("⏸️ DOWN expensive убыток: bid {:.3} <= p_max + 0.03 ({:.3})", down_bid, p_max + 0.03);
        }
    }

    // БАЗОВАЯ СТРАТЕГИЯ: Покупаем только cheap акции
    // Cheap = best_bid < 0.5 (определено в строках 101-102)

    // Проверяем условия для UP стороны
    if up_is_cheap {
        // UP - cheap сторона, покупаем
        info!("✅ UP (cheap): размещаем (up_bid {:.3} < 0.5)", up_bid);
        place_order_on_side(engine, Side::Up, up_bid);
    } else {
        info!("⏸️ UP (expensive): не размещаем (up_bid {:.3} >= 0.5)", up_bid);
    }

    // Проверяем условия для DOWN стороны
    if down_is_cheap {
        // DOWN - cheap сторона, покупаем
        info!("✅ DOWN (cheap): размещаем (down_bid {:.3} < 0.5)", down_bid);
        place_order_on_side(engine, Side::Down, down_bid);
    } else {
        info!("⏸️ DOWN (expensive): не размещаем (down_bid {:.3} >= 0.5)", down_bid);
    }
}

/// Размещает GTD ордер на указанную сторону
fn place_order_on_side(engine: &Arc<RealEngine>, side: Side, price: f64) {
    let size = engine.config.size;

    // Проверяем валидность цены
    if price < 0.02 || price > 0.98 {
        warn!("⚠️ Невалидная цена: {:.3}", price);
        return;
    }

    // Проверяем max_balance
    {
        let port = engine.portfolio.lock().unwrap();
        let total_spent = port.up_spent + port.down_spent;
        if total_spent + (price * size) > engine.config.max_balance {
            info!("📊 Max balance достигнут: {:.2} + {:.2} > {:.2}",
                total_spent, price * size, engine.config.max_balance);
            return;
        }
    }

    let rounded_price = RealEngine::round_price(price);

    info!("🎯 Размещаем {} @ {:.2} | Size: {:.2}",
        if side == Side::Up { "UP" } else { "DOWN" }, rounded_price, size);

    place_gtd_order(engine, side, rounded_price, size);
}

/// Размещает GTD ордер с заданными параметрами
fn place_gtd_order(
    engine: &Arc<RealEngine>,
    side: Side,
    price: f64,
    size: f64,
) {
    let is_up = side == Side::Up;
    let token_id = if is_up {
        Arc::clone(&engine.up_token)
    } else {
        Arc::clone(&engine.down_token)
    };

    let client = engine.client.clone();
    let signer = engine.signer.clone();
    let expiration_seconds = engine.config.expiration_seconds;

    tokio::spawn(async move {
        let price_dec: Decimal = format!("{:.2}", price).parse().unwrap();
        let size_dec: Decimal = format!("{:.2}", size).parse().unwrap();

        // Экспирация: now + expiration_seconds
        let expiration = Utc::now() + chrono::Duration::seconds(60 + expiration_seconds as i64);

        let order = client.limit_order()
            .token_id(token_id.as_ref())
            .price(price_dec)
            .size(size_dec)
            .side(PolySide::Buy)
            .order_type(OrderType::GTD)
            .expiration(expiration)
            .build().await.unwrap();

        let signed = client.sign(&signer, order).await.unwrap();

        match client.post_order(signed).await {
            Ok(response) => {
                if !response.order_id.is_empty() {
                    info!("📝 GTD ордер размещён: {} @ {:.2} | order_id={}",
                        if is_up { "UP" } else { "DOWN" }, price, response.order_id);
                }
            },
            Err(e) => {
                warn!("❌ Ошибка размещения GTD ордера: {}", e);
            },
        }
    });
}

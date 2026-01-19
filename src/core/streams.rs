use std::sync::Arc;
use tracing::{info, warn};
use polymarket_client_sdk::clob::types::{OrderType, Side as PolySide};
use polymarket_client_sdk::types::Decimal;
use chrono::Utc;
use crate::models::Side;
use super::strat::{RealEngine, TradingMode};

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
        return;
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

    // Определяем cheap и expensive стороны
    let up_is_cheap = is_cheap_side(Side::Up, up_bid, up_avg);
    let down_is_cheap = is_cheap_side(Side::Down, down_bid, down_avg);

    info!("📊 Sides: UP {} | DOWN {}",
        if up_is_cheap { "CHEAP" } else { "EXPENSIVE" },
        if down_is_cheap { "CHEAP" } else { "EXPENSIVE" });

    // ДОПОЛНИТЕЛЬНОЕ ПРАВИЛО: Если перекос > size НЕ В ПОЛЬЗУ cheap стороны - закрыть полностью
    // Т.е. если cheap стороны у нас меньше
    let order_size = engine.config.size;
    if skew_abs > order_size {
        // Проверяем: является ли сторона с МЕНЬШИМ количеством акций cheap?
        let skew_against_cheap = if up_shares < down_shares {
            // UP меньше - проверяем, является ли UP cheap
            up_is_cheap
        } else if down_shares < up_shares {
            // DOWN меньше - проверяем, является ли DOWN cheap
            down_is_cheap
        } else {
            false // Нет перекоса
        };

        if skew_against_cheap {
            // Перекос НЕ В ПОЛЬЗУ cheap стороны - закрываем его
            let (buy_side, buy_price, avg_side_with_more) = if up_shares < down_shares {
                // UP меньше (и это cheap), покупаем UP
                (Side::Up, up_bid, down_avg)
            } else {
                // DOWN меньше (и это cheap), покупаем DOWN
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
                info!("💎 Перекос {:.1} акций НЕ В ПОЛЬЗУ CHEAP стороны → Закрываем полностью ({} ордеров)", skew_abs, num_orders);
                for i in 0..num_orders {
                    info!("📝 Ордер {}/{} для закрытия перекоса против cheap: {:?} @ {:.2}",
                        i + 1, num_orders, buy_side, buy_price);
                    place_order_on_side(engine, buy_side, buy_price);
                }
                return;
            }
        }
    }

    // ПРОСТАЯ ЛОГИКА: 3 ключевых правила
    let mut mode = engine.trading_mode.lock().unwrap();

    // Определяем expensive сторону и проверяем ее прибыльность
    let expensive_side_shares = if !up_is_cheap {
        up_shares
    } else if !down_is_cheap {
        down_shares
    } else {
        0.0 // Обе стороны cheap - нет expensive
    };

    let total_spent = {
        let port = engine.portfolio.lock().unwrap();
        port.up_spent + port.down_spent
    };
    let expensive_profitable = expensive_side_shares >= total_spent;

    // ПРАВИЛО 1: avg <= 0.98 → режим BuyExpensive (покупаем пока не выйдем в плюс)
    if total_avg <= 0.99 {
        *mode = TradingMode::BuyExpensive;
        info!("✅ ПРАВИЛО 1: Avg <= 0.98 ({:.3}) → Режим: BuyExpensive (до выхода в плюс)", total_avg);
    }
    // ПРАВИЛО 2: BuyExpensive вышел в плюс → переключение на BuyCheap
    else if *mode == TradingMode::BuyExpensive && expensive_profitable {
        *mode = TradingMode::BuyCheap;
        info!("✅ ПРАВИЛО 2: BuyExpensive вышел в плюс ({:.1} >= {:.1}) → BuyCheap",
            expensive_side_shares, total_spent);
    }
    // ПРАВИЛО 2 продолжение: BuyExpensive работает пока не выйдем в плюс
    else if *mode == TradingMode::BuyExpensive {
        info!("📊 BuyExpensive работает (profit {:.1} < {:.1})", expensive_side_shares, total_spent);
    }
    // ПРАВИЛО 3: BuyCheap ничего не может остановить (работает пока avg > 0.98)
    else if *mode == TradingMode::BuyCheap {
        info!("📊 ПРАВИЛО 3: BuyCheap работает (avg {:.3}, доводим до 0.98)", total_avg);
    }

    // Выполняем действия согласно текущему режиму
    match *mode {
        TradingMode::BuyExpensive => {
            // Покупаем ТОЛЬКО expensive
            if !up_is_cheap {
                place_order_on_side(engine, Side::Up, up_bid);
            }
            if !down_is_cheap {
                place_order_on_side(engine, Side::Down, down_bid);
            }
        }
        TradingMode::BuyCheap => {
            // Покупаем ТОЛЬКО cheap
            if up_is_cheap {
                place_order_on_side(engine, Side::Up, up_bid);
            }
            if down_is_cheap {
                place_order_on_side(engine, Side::Down, down_bid);
            }
        }
    }
}

/// Определяет, является ли сторона cheap (уменьшает avg) или expensive (увеличивает avg)
fn is_cheap_side(_side: Side, current_price: f64, current_avg: f64) -> bool {
    if current_avg == 0.0 {
        // Если avg = 0 (нет позиции), любая покупка УВЕЛИЧИВАЕТ avg → EXPENSIVE
        return false;
    }
    // Cheap если текущая цена меньше avg (уменьшает avg при покупке)
    // Expensive если текущая цена больше avg (увеличивает avg при покупке)
    current_price < current_avg
}

/// Размещает GTD ордер на указанную сторону
fn place_order_on_side(engine: &Arc<RealEngine>, side: Side, price: f64) {
    let size = engine.config.size;

    // Проверяем валидность цены
    if price < 0.01 || price > 0.99 {
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

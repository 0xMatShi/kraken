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
    drop(port);

    info!("📊 Portfolio State: UP {:.1} @ {:.3} | DOWN {:.1} @ {:.3} | Total Avg: {:.3}",
        up_shares, up_avg, down_shares, down_avg, total_avg);

    // Если баланс 0/0 - размещаем на обе стороны одновременно
    if up_shares == 0.0 && down_shares == 0.0 {
        info!("🎯 Начальная фаза (0/0): размещаем на обе стороны");
        place_order_on_side(engine, Side::Up, up_bid);
        place_order_on_side(engine, Side::Down, down_bid);
        return;
    }

    // Определяем cheap и expensive стороны
    let up_is_cheap = is_cheap_side(Side::Up, up_bid, up_avg);
    let down_is_cheap = is_cheap_side(Side::Down, down_bid, down_avg);

    info!("📊 Sides: UP {} | DOWN {}",
        if up_is_cheap { "CHEAP" } else { "EXPENSIVE" },
        if down_is_cheap { "CHEAP" } else { "EXPENSIVE" });

    // Логика предпочтений на основе total_avg
    if total_avg >= 1.02 {
        // Предпочтение cheap стороне
        info!("✅ Total Avg >= 1.02 → Предпочтение CHEAP стороне");
        if up_is_cheap {
            place_order_on_side(engine, Side::Up, up_bid);
        }
        if down_is_cheap {
            place_order_on_side(engine, Side::Down, down_bid);
        }
    } else if total_avg <= 0.98 {
        // Предпочтение expensive стороне
        info!("✅ Total Avg <= 0.98 → Предпочтение EXPENSIVE стороне");
        if !up_is_cheap {
            place_order_on_side(engine, Side::Up, up_bid);
        }
        if !down_is_cheap {
            place_order_on_side(engine, Side::Down, down_bid);
        }
    } else {
        // total_avg между 0.99 и 1.01 - размещаем на обе стороны
        info!("📊 Total Avg между 0.99 и 1.01 → Размещаем на обе стороны");
        place_order_on_side(engine, Side::Up, up_bid);
        place_order_on_side(engine, Side::Down, down_bid);
    }
}

/// Определяет, является ли сторона cheap (уменьшает avg) или expensive (увеличивает avg)
fn is_cheap_side(_side: Side, current_price: f64, current_avg: f64) -> bool {
    if current_avg == 0.0 {
        // Если avg = 0 (нет позиции), то считаем cheap если цена < 0.5
        return current_price < 0.5;
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

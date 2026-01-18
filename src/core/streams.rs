use std::sync::Arc;
use tracing::{info, warn};
use polymarket_client_sdk::clob::types::{OrderType, Side as PolySide};
use polymarket_client_sdk::types::Decimal;
use chrono::Utc;
use crate::models::Side;
use super::strat::{RealEngine, StreamType, ActiveOrder};

/// Stream 1: Cheap Side Logic
/// Размещает GTD ордера на стороне с bid < 0.5
pub fn run_cheap_side_stream(engine: &Arc<RealEngine>, side: Side, bid_price: f64) {
    // Проверяем валидность цены
    if bid_price < 0.01 || bid_price >= 0.50 {
        return;
    }

    let size = engine.config.size;

    // Рассчитываем эффективный лимит с учетом перекоса
    let (effective_limit, directed_skew) = {
        let port = engine.portfolio.lock().unwrap();
        let skew = port.directed_skew(side);

        // Если перекос отрицательный (больше expensive акций), увеличиваем лимит
        let eff_limit = if skew < 0.0 {
            engine.config.cheap_limit + skew.abs()
        } else {
            engine.config.cheap_limit
        };

        (eff_limit, skew)
    };

    // Проверяем виртуальный лимит с учетом эффективного лимита
    {
        let limit = engine.virtual_limit.lock().unwrap();
        let available = effective_limit - limit.used_shares;
        if available < size {
            info!("📊 Stream 1 | Виртуальный лимит исчерпан: {:.1}/{:.1} (base: {:.1}, skew: {:.1})",
                limit.used_shares, effective_limit, engine.config.cheap_limit, directed_skew);
            return;
        }
    }

    // Проверяем max_balance
    {
        let port = engine.portfolio.lock().unwrap();
        let total_spent = port.up_spent + port.down_spent;
        if total_spent + (bid_price * size) > engine.config.max_balance {
            info!("📊 Stream 1 | Max balance достигнут");
            return;
        }
    }

    // Резервируем shares в виртуальном лимите
    {
        let mut limit = engine.virtual_limit.lock().unwrap();
        limit.used_shares += size;
        info!("📊 Stream 1 | Зарезервировано: {:.1} | Использовано: {:.1}/{:.1}",
            size, limit.used_shares, engine.config.cheap_limit);
    }

    let rounded_price = RealEngine::round_price(bid_price);

    info!("🎯 Stream 1 (Cheap) | Размещаем {} @ {:.2} | Size: {:.2}",
        if side == Side::Up { "UP" } else { "DOWN" }, rounded_price, size);

    place_gtd_order(engine, side, rounded_price, size, StreamType::CheapSide);
}

/// Stream 2: Expensive Side Logic
/// Ratio-based логика для закрытия перекоса на стороне с bid > 0.5
pub fn run_expensive_side_stream(
    engine: &Arc<RealEngine>,
    cheap_side: Side,
    _cheap_bid: f64,
    expensive_side: Side,
    expensive_bid: f64,
) {
    // Проверяем валидность цены
    if expensive_bid <= 0.50 || expensive_bid > 0.99 {
        return;
    }

    let size = engine.config.size;

    // Получаем информацию о портфеле с учетом направления перекоса
    let (directed_skew, cheap_avg) = {
        let port = engine.portfolio.lock().unwrap();
        let skew = port.directed_skew(cheap_side);
        let (_, avg) = port.cheap_side_info(cheap_side);
        (skew, avg)
    };

    // Stream 2 работает ТОЛЬКО если перекос положительный (больше cheap акций)
    if directed_skew <= 0.0 {
        return; // Больше expensive акций или равно - Stream 2 не работает
    }

    // Если перекос меньше size - слишком мал для закрытия
    if directed_skew < size {
        return; // Не размещаем ничего
    }

    // Рассчитываем ratio
    // ratio = (1 - cheap_avg) / expensive_bid
    let ratio = if expensive_bid > 0.0 && cheap_avg > 0.0 {
        (1.0 - cheap_avg) / expensive_bid
    } else {
        0.0
    };

    info!("📊 Stream 2 | Ratio: {:.4} = (1 - {:.2}) / {:.2} | Skew: {:.1}",
        ratio, cheap_avg, expensive_bid, directed_skew);

    if ratio > 1.00 {
        // Profitable to close skew
        info!("✅ Stream 2 | Ratio > 1.00 → Закрываем skew {:.1}",
            directed_skew);

        // Проверяем max_balance
        {
            let port = engine.portfolio.lock().unwrap();
            let total_spent = port.up_spent + port.down_spent;
            if total_spent + (expensive_bid * size) > engine.config.max_balance {
                return;
            }
        }

        let rounded_price = RealEngine::round_price(expensive_bid);

        // Размещаем только 1 ордер за тик (остальные при следующих тиках)
        info!("🎯 Stream 2 (Close Skew) | Размещаем {} @ {:.2} | Size: {:.2}",
            if expensive_side == Side::Up { "UP" } else { "DOWN" }, rounded_price, size);

        // Освобождаем виртуальный лимит при закрытии skew
        {
            let mut limit = engine.virtual_limit.lock().unwrap();
            let release = size.min(limit.used_shares);
            limit.used_shares -= release;
            info!("📊 Stream 2 | Освобождено из лимита: {:.1} | Осталось: {:.1}/{:.1}",
                release, limit.used_shares, engine.config.cheap_limit);
        }

        place_gtd_order(engine, expensive_side, rounded_price, size, StreamType::ExpensiveSide);
    } else if ratio >= 0.99 {
        // Deadband: 0.99 <= ratio <= 1.00
        info!("⏸️ Stream 2 | Deadband (0.99-1.00): ничего не делаем");
    } else {
        // ratio < 0.99 - не выгодно закрывать перекос
        info!("📊 Stream 2 | Ratio < 0.99 → ждем роста цены expensive side");
    }
}

/// Размещает GTD ордер с заданными параметрами
fn place_gtd_order(
    engine: &Arc<RealEngine>,
    side: Side,
    price: f64,
    size: f64,
    stream_type: StreamType,
) {
    let is_up = side == Side::Up;
    let token_id = if is_up {
        Arc::clone(&engine.up_token)
    } else {
        Arc::clone(&engine.down_token)
    };

    let client = engine.client.clone();
    let signer = engine.signer.clone();
    let engine_clone = Arc::clone(engine);
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

                    // Добавляем в active_orders
                    let mut orders = engine_clone.active_orders.lock().unwrap();
                    orders.insert(response.order_id.clone(), ActiveOrder {
                        order_id: response.order_id,
                        is_up,
                        price,
                        size,
                        filled: 0.0,
                        stream_type,
                    });
                }
            },
            Err(e) => {
                warn!("❌ Ошибка размещения GTD ордера: {}", e);
                // Освобождаем виртуальный лимит если это был CheapSide ордер
                if matches!(stream_type, StreamType::CheapSide) {
                    let mut limit = engine_clone.virtual_limit.lock().unwrap();
                    limit.used_shares = (limit.used_shares - size).max(0.0);
                    info!("📊 Освобождено из лимита после ошибки: {:.1}", size);
                }
            },
        }
    });
}

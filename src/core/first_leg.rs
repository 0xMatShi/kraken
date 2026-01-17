use std::sync::{Arc, Mutex};
use tracing::info;
use polymarket_client_sdk::clob::types::{OrderType, Side as PolySide};
use polymarket_client_sdk::types::Decimal;
use chrono::Utc;
use crate::models::MarketPrices;
use super::strat::{RealEngine, TradingState, price_to_cents, ReservedPriceKey};

/// Пытаемся разместить первую ногу для конкретного потока
/// thread_idx - индекс потока (для логирования)
/// is_up_side = true → покупаем UP (UP-поток), вторая нога будет DOWN
/// is_up_side = false → покупаем DOWN (DOWN-поток), вторая нога будет UP
/// trading_state - состояние конкретного потока
pub fn try_place_first_leg_for_thread(
    engine: &RealEngine,
    thread_idx: usize,
    prices: &MarketPrices,
    is_up_side: bool,
    trading_state: &Arc<Mutex<TradingState>>,
) {
    // Проверяем валидность цен
    if prices.up_bid < 0.01 || prices.down_bid < 0.01 {
        return;
    }

    // Определяем цену первой ноги в зависимости от стороны
    let (first_leg_is_up, first_leg_price, bid_price) = if is_up_side {
        // Покупаем UP только если up_bid > 0.51 и < 0.98
        if prices.up_bid <= 0.59 || prices.up_bid >= 0.98 {
            return;
        }
        (true, RealEngine::round_price(prices.up_bid), prices.up_bid)
    } else {
        // Покупаем DOWN только если down_bid > 0.51 и < 0.98
        if prices.down_bid <= 0.59 || prices.down_bid >= 0.98 {
            return;
        }
        (false, RealEngine::round_price(prices.down_bid), prices.down_bid)
    };

    if first_leg_price < 0.01 || first_leg_price > 0.99 {
        return;
    }

    // Определяем размер ордера в зависимости от цены
    let order_size = if bid_price >= 0.75 {
        engine.config.size * 1.25
    } else if bid_price >= 0.65 {
        engine.config.size * 1.125
    } else if bid_price >= 0.60 {
        engine.config.size * 1.0625
    } else {
        engine.config.size
    };

    // Проверяем, не забронирована ли эта цена другим потоком
    let price_key: ReservedPriceKey = (first_leg_is_up, price_to_cents(first_leg_price));
    {
        let reserved = engine.reserved_prices.lock().unwrap();
        if reserved.contains(&price_key) {
            return;
        }
    }

    // Бронируем цену перед размещением
    {
        let mut reserved = engine.reserved_prices.lock().unwrap();
        reserved.insert(price_key);
    }

    let stream_type = if is_up_side { "UP" } else { "DOWN" };
    info!("🎯 {}-поток #{} | Спред 2с найден! Размещаем первую ногу: {} @ {:.2} (bid={:.2})",
        stream_type, thread_idx + 1, if first_leg_is_up { "UP" } else { "DOWN" }, first_leg_price, bid_price);

    // Переходим в состояние ожидания сразу
    *trading_state.lock().unwrap() = TradingState::WaitingFirstLeg {
        order_id: String::new(),
        is_up: first_leg_is_up,
        price: first_leg_price,
        size: order_size,
    };

    let trading_state_clone = Arc::clone(trading_state);
    let token_id = if first_leg_is_up {
        Arc::clone(&engine.up_token)
    } else {
        Arc::clone(&engine.down_token)
    };
    let client = engine.client.clone();
    let signer = engine.signer.clone();
    let size = order_size;

    tokio::spawn(async move {
        let price_dec: Decimal = format!("{:.2}", first_leg_price).parse().unwrap();
        let size_dec: Decimal = format!("{:.2}", size).parse().unwrap();

        // Экспирация: now + 63 секунды
        let expiration = Utc::now() + chrono::Duration::seconds(63);

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
                    info!("📝 Первая нога размещена: order_id={}", response.order_id);

                    let mut state = trading_state_clone.lock().unwrap();
                    if let TradingState::WaitingFirstLeg { ref mut order_id, .. } = *state {
                        *order_id = response.order_id;
                    }
                }
            },
            Err(e) => tracing::error!("❌ Ошибка размещения первой ноги: {}", e),
        }
    });
}

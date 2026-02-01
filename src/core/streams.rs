use super::strat::RealEngine;
use crate::models::Side;
use polymarket_client_sdk::clob::types::{OrderType, Side as PolySide};
use polymarket_client_sdk::types::Decimal;
use std::sync::Arc;
use tracing::{info, warn};

/// Размещает первую ногу торговой пары
///
/// Вызывается из process_tick() после определения тренда и целевой цены
pub fn place_first_leg(engine: &Arc<RealEngine>, side: Side, price: f64, size: f64) {
    let is_up = matches!(side, Side::Up);

    let token_id = if is_up {
        Arc::clone(&engine.up_token)
    } else {
        Arc::clone(&engine.down_token)
    };

    let client = engine.client.clone();
    let signer = engine.signer.clone();
    let engine_clone = Arc::clone(engine);
    let side_clone = side;

    tokio::spawn(async move {
        let rounded_price = RealEngine::round_price(price);
        let price_dec: Decimal = format!("{:.2}", rounded_price).parse().unwrap();
        let size_dec: Decimal = format!("{:.2}", size).parse().unwrap();

        // GTC ордер - без экспирации
        let order = match client
            .limit_order()
            .token_id(token_id.as_ref())
            .price(price_dec)
            .size(size_dec)
            .side(PolySide::Buy)
            .order_type(OrderType::GTC)
            .build()
            .await
        {
            Ok(o) => o,
            Err(e) => {
                warn!("❌ Ошибка создания ордера первой ноги: {}", e);
                return;
            }
        };

        let signed = match client.sign(&signer, order).await {
            Ok(s) => s,
            Err(e) => {
                warn!("❌ Ошибка подписи ордера первой ноги: {}", e);
                return;
            }
        };

        match client.post_order(signed).await {
            Ok(response) => {
                if !response.order_id.is_empty() {
                    info!(
                        "📝 ПЕРВАЯ НОГА размещена: {:?} @ {:.2} | order_id={}",
                        side_clone, rounded_price, response.order_id
                    );

                    // Регистрируем первую ногу в engine
                    engine_clone.register_first_leg(
                        response.order_id,
                        side_clone,
                        rounded_price,
                        size,
                    );
                } else {
                    warn!("⚠️ Ордер первой ноги размещен но order_id пустой");
                }
            }
            Err(e) => {
                warn!(
                    "❌ Ошибка размещения первой ноги {:?} @ {:.2}: {}",
                    side_clone, rounded_price, e
                );
            }
        }
    });
}

/// Размещает вторую ногу торговой пары
///
/// Вызывается после полного исполнения первой ноги (из handle.rs через engine)
pub fn place_second_leg(
    engine: &Arc<RealEngine>,
    first_leg_order_id: String,
    side: Side,
    price: f64,
    size: f64,
    first_leg_price: f64,
    timer_interval_secs: u64,
) {
    let is_up = matches!(side, Side::Up);

    let token_id = if is_up {
        Arc::clone(&engine.up_token)
    } else {
        Arc::clone(&engine.down_token)
    };

    let client = engine.client.clone();
    let signer = engine.signer.clone();
    let engine_clone = Arc::clone(engine);
    let side_clone = side;

    tokio::spawn(async move {
        let rounded_price = RealEngine::round_price(price);
        let price_dec: Decimal = format!("{:.2}", rounded_price).parse().unwrap();
        let size_dec: Decimal = format!("{:.2}", size).parse().unwrap();

        // GTC ордер - без экспирации
        let order = match client
            .limit_order()
            .token_id(token_id.as_ref())
            .price(price_dec)
            .size(size_dec)
            .side(PolySide::Buy)
            .order_type(OrderType::GTC)
            .build()
            .await
        {
            Ok(o) => o,
            Err(e) => {
                warn!("❌ Ошибка создания ордера второй ноги: {}", e);
                return;
            }
        };

        let signed = match client.sign(&signer, order).await {
            Ok(s) => s,
            Err(e) => {
                warn!("❌ Ошибка подписи ордера второй ноги: {}", e);
                return;
            }
        };

        match client.post_order(signed).await {
            Ok(response) => {
                if !response.order_id.is_empty() {
                    info!(
                        "📝 ВТОРАЯ НОГА размещена: {:?} @ {:.2} | order_id={} (first_leg: {})",
                        side_clone, rounded_price, response.order_id, first_leg_order_id
                    );

                    // Регистрируем вторую ногу в engine
                    engine_clone.register_second_leg(
                        &first_leg_order_id,
                        response.order_id,
                        side_clone,
                        rounded_price,
                        size,
                        first_leg_price,
                        timer_interval_secs,
                    );
                } else {
                    warn!("⚠️ Ордер второй ноги размещен но order_id пустой");
                }
            }
            Err(e) => {
                warn!(
                    "❌ Ошибка размещения второй ноги {:?} @ {:.2}: {}",
                    side_clone, rounded_price, e
                );
            }
        }
    });
}

/// Размещает cumulative первую ногу батчевым запросом
pub fn place_cumulative_first_leg(
    engine: &Arc<RealEngine>,
    side: Side,
    price: f64,
    sizes: Vec<f64>,
) {
    if sizes.is_empty() {
        return;
    }

    let is_up = matches!(side, Side::Up);

    let token_id = if is_up {
        Arc::clone(&engine.up_token)
    } else {
        Arc::clone(&engine.down_token)
    };

    let client = engine.client.clone();
    let signer = engine.signer.clone();
    let engine_clone = Arc::clone(engine);
    let side_clone = side;

    tokio::spawn(async move {
        let rounded_price = RealEngine::round_price(price);
        let price_dec: Decimal = format!("{:.2}", rounded_price).parse().unwrap();

        // Создаем и подписываем все ордера
        let mut signed_orders = Vec::new();

        for size in &sizes {
            let size_dec: Decimal = format!("{:.2}", size).parse().unwrap();

            let order = match client
                .limit_order()
                .token_id(token_id.as_ref())
                .price(price_dec)
                .size(size_dec)
                .side(PolySide::Buy)
                .order_type(OrderType::GTC)
                .build()
                .await
            {
                Ok(o) => o,
                Err(e) => {
                    warn!(
                        "❌ [Cumulative] Ошибка создания ордера первой ноги size={:.2}: {}",
                        size, e
                    );
                    continue;
                }
            };

            let signed = match client.sign(&signer, order).await {
                Ok(s) => s,
                Err(e) => {
                    warn!(
                        "❌ [Cumulative] Ошибка подписи ордера первой ноги size={:.2}: {}",
                        size, e
                    );
                    continue;
                }
            };

            signed_orders.push(signed);
        }

        if signed_orders.is_empty() {
            warn!("⚠️ [Cumulative] Не удалось подготовить ни одного ордера первой ноги");
            return;
        }

        // Размещаем все ордера батчевым запросом
        match client.post_orders(signed_orders).await {
            Ok(responses) => {
                for response in responses {
                    if !response.order_id.is_empty() {
                        engine_clone.register_cumulative_first_leg(response.order_id);
                    }
                }
            }
            Err(e) => {
                warn!(
                    "❌ [Cumulative] Ошибка батчевого размещения первой ноги {:?} @ {:.2}: {}",
                    side_clone, rounded_price, e
                );
            }
        }
    });
}

/// Размещает cumulative вторую ногу батчевым запросом
pub fn place_cumulative_second_leg(
    engine: &Arc<RealEngine>,
    side: Side,
    price: f64,
    sizes: Vec<f64>,
) {
    if sizes.is_empty() {
        return;
    }

    let is_up = matches!(side, Side::Up);

    let token_id = if is_up {
        Arc::clone(&engine.up_token)
    } else {
        Arc::clone(&engine.down_token)
    };

    let client = engine.client.clone();
    let signer = engine.signer.clone();
    let engine_clone = Arc::clone(engine);
    let side_clone = side;

    tokio::spawn(async move {
        let rounded_price = RealEngine::round_price(price);
        let price_dec: Decimal = format!("{:.2}", rounded_price).parse().unwrap();

        // Создаем и подписываем все ордера
        let mut signed_orders = Vec::new();

        for size in &sizes {
            let size_dec: Decimal = format!("{:.2}", size).parse().unwrap();

            let order = match client
                .limit_order()
                .token_id(token_id.as_ref())
                .price(price_dec)
                .size(size_dec)
                .side(PolySide::Buy)
                .order_type(OrderType::GTC)
                .build()
                .await
            {
                Ok(o) => o,
                Err(e) => {
                    warn!(
                        "❌ [Cumulative] Ошибка создания ордера второй ноги size={:.2}: {}",
                        size, e
                    );
                    continue;
                }
            };

            let signed = match client.sign(&signer, order).await {
                Ok(s) => s,
                Err(e) => {
                    warn!(
                        "❌ [Cumulative] Ошибка подписи ордера второй ноги size={:.2}: {}",
                        size, e
                    );
                    continue;
                }
            };

            signed_orders.push(signed);
        }

        if signed_orders.is_empty() {
            warn!("⚠️ [Cumulative] Не удалось подготовить ни одного ордера второй ноги");
            return;
        }

        // Размещаем все ордера батчевым запросом
        match client.post_orders(signed_orders).await {
            Ok(responses) => {
                for response in responses {
                    if !response.order_id.is_empty() {
                        engine_clone.register_cumulative_second_leg(response.order_id);
                    }
                }
            }
            Err(e) => {
                warn!(
                    "❌ [Cumulative] Ошибка батчевого размещения второй ноги {:?} @ {:.2}: {}",
                    side_clone, rounded_price, e
                );
            }
        }
    });
}

/// Отменяет ордер по order_id
pub fn cancel_order(engine: &Arc<RealEngine>, order_id: String) {
    let client = engine.client.clone();

    tokio::spawn(async move {
        match client.cancel_order(&order_id).await {
            Ok(_) => {
                info!("🗑️ Ордер отменен: {}", order_id);
            }
            Err(e) => {
                warn!("⚠️ Ошибка отмены ордера {}: {}", order_id, e);
            }
        }
    });
}

/// Отменяет несколько ордеров батчевым запросом
pub fn cancel_orders(engine: &Arc<RealEngine>, order_ids: Vec<String>) {
    if order_ids.is_empty() {
        return;
    }

    let client = engine.client.clone();
    let count = order_ids.len();

    tokio::spawn(async move {
        let order_ids_refs: Vec<&str> = order_ids.iter().map(|s| s.as_str()).collect();

        match client.cancel_orders(&order_ids_refs).await {
            Ok(_) => {}
            Err(e) => {
                warn!("⚠️ Ошибка батчевой отмены {} ордеров: {}", count, e);
            }
        }
    });
}

/// Размещает hedge ордер (покупка по рынку через GTC limit @ 0.99)
///
/// Используется для ручного хеджирования позиции в режиме Hedge
pub fn place_hedge_order(engine: &Arc<RealEngine>, side: Side, size: f64) {
    let is_up = matches!(side, Side::Up);

    let token_id = if is_up {
        Arc::clone(&engine.up_token)
    } else {
        Arc::clone(&engine.down_token)
    };

    let client = engine.client.clone();
    let signer = engine.signer.clone();

    tokio::spawn(async move {
        // Цена 0.99 гарантирует моментальное исполнение как taker
        let price = 0.99;
        let price_dec: Decimal = format!("{:.2}", price).parse().unwrap();
        let size_dec: Decimal = format!("{:.2}", size).parse().unwrap();

        // GTC ордер - без экспирации
        let order = match client
            .limit_order()
            .token_id(token_id.as_ref())
            .price(price_dec)
            .size(size_dec)
            .side(PolySide::Buy)
            .order_type(OrderType::GTC)
            .build()
            .await
        {
            Ok(o) => o,
            Err(e) => {
                warn!("❌ Ошибка создания hedge ордера: {}", e);
                return;
            }
        };

        let signed = match client.sign(&signer, order).await {
            Ok(s) => s,
            Err(e) => {
                warn!("❌ Ошибка подписи hedge ордера: {}", e);
                return;
            }
        };

        match client.post_order(signed).await {
            Ok(response) => {
                if !response.order_id.is_empty() {
                    info!(
                        "🛡️ HEDGE ордер размещен: {:?} @ {:.2} | Size: {:.2} | order_id={}",
                        side, price, size, response.order_id
                    );
                } else {
                    warn!("⚠️ Hedge ордер размещен но order_id пустой");
                }
            }
            Err(e) => {
                warn!(
                    "❌ Ошибка размещения hedge ордера {:?} @ {:.2}: {}",
                    side, price, e
                );
            }
        }
    });
}

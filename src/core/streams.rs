use std::sync::Arc;
use tracing::{info, warn};
use polymarket_client_sdk::clob::types::{OrderType, Side as PolySide};
use polymarket_client_sdk::types::Decimal;
use crate::models::Side;
use super::strat::RealEngine;

/// Размещает первую ногу торговой пары
/// 
/// Вызывается из process_tick() после определения тренда и целевой цены
pub fn place_first_leg(
    engine: &Arc<RealEngine>,
    side: Side,
    price: f64,
    size: f64,
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
        let order = match client.limit_order()
            .token_id(token_id.as_ref())
            .price(price_dec)
            .size(size_dec)
            .side(PolySide::Buy)
            .order_type(OrderType::GTC)
            .build().await {
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
                    info!("📝 ПЕРВАЯ НОГА размещена: {:?} @ {:.2} | order_id={}",
                        side_clone, rounded_price, response.order_id);

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
            },
            Err(e) => {
                warn!("❌ Ошибка размещения первой ноги {:?} @ {:.2}: {}", side_clone, rounded_price, e);
            },
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
        let order = match client.limit_order()
            .token_id(token_id.as_ref())
            .price(price_dec)
            .size(size_dec)
            .side(PolySide::Buy)
            .order_type(OrderType::GTC)
            .build().await {
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
                    info!("📝 ВТОРАЯ НОГА размещена: {:?} @ {:.2} | order_id={} (first_leg: {})",
                        side_clone, rounded_price, response.order_id, first_leg_order_id);

                    // Регистрируем вторую ногу в engine
                    engine_clone.register_second_leg(
                        &first_leg_order_id,
                        response.order_id,
                        side_clone,
                        rounded_price,
                        size,
                        first_leg_price,
                    );
                } else {
                    warn!("⚠️ Ордер второй ноги размещен но order_id пустой");
                }
            },
            Err(e) => {
                warn!("❌ Ошибка размещения второй ноги {:?} @ {:.2}: {}", side_clone, rounded_price, e);
            },
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
            },
            Err(e) => {
                warn!("⚠️ Ошибка отмены ордера {}: {}", order_id, e);
            },
        }
    });
}

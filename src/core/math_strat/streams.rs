use super::super::strat::RealEngine;
use crate::models::Side;
use polymarket_client_sdk::clob::types::{OrderType, Side as PolySide};
use polymarket_client_sdk::types::Decimal;
use std::sync::Arc;
use tracing::{info, warn};

/// Размещает Math ордер на указанной стороне по цене уровня
pub fn place_math_order(engine: &Arc<RealEngine>, side: Side, price: f64, size: f64, level: u8) {
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
                warn!("❌ [Math] Ошибка создания ордера: {}", e);
                engine_clone.cancel_pending_level(side_clone, level);
                return;
            }
        };

        let signed = match client.sign(&signer, order).await {
            Ok(s) => s,
            Err(e) => {
                warn!("❌ [Math] Ошибка подписи ордера: {}", e);
                engine_clone.cancel_pending_level(side_clone, level);
                return;
            }
        };

        match client.post_orders(vec![signed]).await {
            Ok(responses) => {
                let mut registered = false;
                for response in responses {
                    if response.success && !response.order_id.is_empty() {
                        info!(
                            "📝 [Math] Ордер размещён: {:?} @ {:.2} уровень {} | order_id={}",
                            side_clone, rounded_price, level, response.order_id
                        );
                        engine_clone.register_math_order(
                            response.order_id,
                            side_clone,
                            rounded_price,
                            size,
                            level,
                        );
                        registered = true;
                    } else {
                        if let Some(err) = &response.error_msg {
                            warn!("❌ [Math] Ордер отклонён: {}", err);
                        }
                    }
                }
                if !registered {
                    engine_clone.cancel_pending_level(side_clone, level);
                }
            }
            Err(e) => {
                warn!(
                    "❌ [Math] Ошибка размещения ордера {:?} @ {:.2}: {}",
                    side_clone, rounded_price, e
                );
                engine_clone.cancel_pending_level(side_clone, level);
            }
        }
    });
}

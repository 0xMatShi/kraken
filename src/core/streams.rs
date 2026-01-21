use std::sync::Arc;
use tracing::{info, warn};
use polymarket_client_sdk::clob::types::{OrderType, Side as PolySide};
use polymarket_client_sdk::types::Decimal;
use crate::models::MarketPrices;
use super::strat::RealEngine;

/// Новая логика размещения лимиток
/// Размещает 2 GTC лимитки одновременно при выполнении условий:
/// - сумма бест бидов <= 0.99
/// - размер на обеих сторонах 50 < size < 100
/// - max_balance не превышен
/// + отменяет и заменяет неисполненные лимитки
pub fn process_order_placement(engine: &Arc<RealEngine>, prices: MarketPrices) {
    let up_bid = prices.up_bid;
    let down_bid = prices.down_bid;
    let up_size = prices.up_bid_size;
    let down_size = prices.down_bid_size;

    info!("📊 Тик: UP bid {:.3} (size {:.1}) | DOWN bid {:.3} (size {:.1})",
        up_bid, up_size, down_bid, down_size);

    // Условие 1: Сумма бест бидов <= 0.99
    let sum_bids = up_bid + down_bid;
    if sum_bids > 0.99 {
        info!("⏸️ Сумма бидов {:.3} > 0.99 - не размещаем", sum_bids);
        return;
    }

    // Условие 2: Размер на UP стороне 50 < size < 100
    if up_size <= 50.0 || up_size >= 100.0 {
        info!("⏸️ UP size {:.1} вне диапазона (50, 100) - не размещаем", up_size);
        return;
    }

    // Условие 3: Размер на DOWN стороне 50 < size < 100
    if down_size <= 50.0 || down_size >= 100.0 {
        info!("⏸️ DOWN size {:.1} вне диапазона (50, 100) - не размещаем", down_size);
        return;
    }

    // Условие 4: Проверяем max_balance
    let order_size = engine.config.size;
    let total_cost = (up_bid + down_bid) * order_size;
    {
        let port = engine.portfolio.lock().unwrap();
        let total_spent = port.up_spent + port.down_spent;
        if total_spent + total_cost > engine.config.max_balance {
            info!("⏸️ Max balance достигнут: {:.2} + {:.2} > {:.2}",
                total_spent, total_cost, engine.config.max_balance);
            return;
        }
    }

    // Получаем список неисполненных ордеров с ценами (это КЛОНЫ, не ссылки!)
    let (pending_up, pending_down) = engine.get_pending_orders();

    // Фильтруем ордера: размещаем замену только если новая цена ВЫШЕ старой
    let up_replacements: Vec<_> = pending_up.iter()
        .filter(|(_, old_price)| up_bid > *old_price)
        .collect();
    let down_replacements: Vec<_> = pending_down.iter()
        .filter(|(_, old_price)| down_bid > *old_price)
        .collect();

    let up_replace_count = up_replacements.len();
    let down_replace_count = down_replacements.len();

    if !pending_up.is_empty() || !pending_down.is_empty() {
        info!("🔄 Обнаружены неисполненные ордера: UP={} DOWN={}", pending_up.len(), pending_down.len());

        // КРИТИЧЕСКИ ВАЖНО: Очищаем оригинальные pending списки СРАЗУ!
        engine.clear_pending_orders();

        // Отменяем все неисполненные ордера асинхронно
        for (order_id, old_price) in pending_up.iter().chain(pending_down.iter()) {
            let is_up = pending_up.iter().any(|(id, _)| id == order_id);
            let new_price = if is_up { up_bid } else { down_bid };

            if new_price > *old_price {
                info!("🔄 Отменяем неисполненный ордер: {} @ {:.3} (новая цена {:.3} выше)",
                    if is_up { "UP" } else { "DOWN" }, old_price, new_price);
                cancel_order(engine, order_id.clone());
            } else {
                info!("⏭️ Скипаем замену: {} @ {:.3} (новая цена {:.3} не выше - ордер скорее всего исполнен)",
                    if is_up { "UP" } else { "DOWN" }, old_price, new_price);
            }
        }
    }

    info!("✅ Условия выполнены! Размещаем основную пару + {} замен (UP={}, DOWN={})",
        up_replace_count + down_replace_count, up_replace_count, down_replace_count);

    // Размещаем основную пару (UP + DOWN)
    place_single_order(engine, true, up_bid, order_size);
    place_single_order(engine, false, down_bid, order_size);

    // Размещаем дополнительные лимитки только для ордеров с ценой ниже новой
    for _ in 0..up_replace_count {
        info!("🔁 Размещаем дополнительную UP лимитку (замена неисполненной)");
        place_single_order(engine, true, up_bid, order_size);
    }

    for _ in 0..down_replace_count {
        info!("🔁 Размещаем дополнительную DOWN лимитку (замена неисполненной)");
        place_single_order(engine, false, down_bid, order_size);
    }
}

/// Размещает один GTC ордер через post_order
fn place_single_order(
    engine: &Arc<RealEngine>,
    is_up: bool,
    price: f64,
    size: f64,
) {
    let token_id = if is_up {
        Arc::clone(&engine.up_token)
    } else {
        Arc::clone(&engine.down_token)
    };

    let client = engine.client.clone();
    let signer = engine.signer.clone();
    let engine_clone = Arc::clone(engine);

    tokio::spawn(async move {
        let rounded_price = RealEngine::round_price(price);
        let price_dec: Decimal = format!("{:.2}", rounded_price).parse().unwrap();
        let size_dec: Decimal = format!("{:.2}", size).parse().unwrap();

        // GTC ордер - без экспирации
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
                    info!("📝 GTC лимитка размещена: {} @ {:.3} | order_id={}",
                        if is_up { "UP" } else { "DOWN" }, rounded_price, response.order_id);

                    // Добавляем в список неисполненных с ценой (будет удален при FILL)
                    engine_clone.add_pending_order(response.order_id, is_up, rounded_price);
                } else {
                    warn!("⚠️ Ордер {} размещен но order_id пустой", if is_up { "UP" } else { "DOWN" });
                }
            },
            Err(e) => {
                warn!("❌ Ошибка размещения GTC лимитки {}: {}", if is_up { "UP" } else { "DOWN" }, e);
            },
        }
    });
}

/// Отменяет ордер по order_id
fn cancel_order(engine: &Arc<RealEngine>, order_id: String) {
    let client = engine.client.clone();

    tokio::spawn(async move {
        match client.cancel_order(&order_id).await {
            Ok(_) => {
                info!("🗑️ Неисполненный ордер отменен: {}", order_id);
            },
            Err(e) => {
                warn!("⚠️ Ошибка отмены ордера {}: {}", order_id, e);
            },
        }
    });
}

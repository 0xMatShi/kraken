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
/// - счетчик активных лимиток < max_active_orders
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

    // Условие 4: Проверяем лимит активных лимиток
    if !engine.can_place_order() {
        let count = engine.active_orders_count.lock().unwrap();
        info!("⏸️ Достигнут лимит активных лимиток: {}/{}", *count, engine.config.max_active_orders);
        return;
    }

    // Условие 5: Проверяем max_balance
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

    // Все условия выполнены - размещаем ДВЕ GTC лимитки одновременно
    info!("✅ Условия выполнены! Размещаем 2 GTC лимитки: UP @ {:.3} + DOWN @ {:.3}", up_bid, down_bid);

    // Увеличиваем счетчик ПЕРЕД размещением, чтобы избежать race condition
    engine.increment_orders_count();

    // Вызываем асинхронную функцию для размещения обоих ордеров
    place_both_orders(engine, up_bid, down_bid, order_size);
}

/// Размещает оба GTC ордера одним запросом через post_orders
fn place_both_orders(
    engine: &Arc<RealEngine>,
    up_price: f64,
    down_price: f64,
    size: f64,
) {
    let up_token = Arc::clone(&engine.up_token);
    let down_token = Arc::clone(&engine.down_token);
    let client = engine.client.clone();
    let signer = engine.signer.clone();
    let engine_clone = Arc::clone(engine);

    tokio::spawn(async move {
        // Округляем цены
        let up_rounded = RealEngine::round_price(up_price);
        let down_rounded = RealEngine::round_price(down_price);

        let up_price_dec: Decimal = format!("{:.2}", up_rounded).parse().unwrap();
        let down_price_dec: Decimal = format!("{:.2}", down_rounded).parse().unwrap();
        let size_dec: Decimal = format!("{:.2}", size).parse().unwrap();

        // Создаем UP ордер
        let up_order = client.limit_order()
            .token_id(up_token.as_ref())
            .price(up_price_dec)
            .size(size_dec)
            .side(PolySide::Buy)
            .order_type(OrderType::GTC)
            .build().await.unwrap();

        // Создаем DOWN ордер
        let down_order = client.limit_order()
            .token_id(down_token.as_ref())
            .price(down_price_dec)
            .size(size_dec)
            .side(PolySide::Buy)
            .order_type(OrderType::GTC)
            .build().await.unwrap();

        // Подписываем оба ордера
        let up_signed = client.sign(&signer, up_order).await.unwrap();
        let down_signed = client.sign(&signer, down_order).await.unwrap();

        // Размещаем оба ордера одним запросом
        let orders = vec![up_signed, down_signed];

        match client.post_orders(orders).await {
            Ok(responses) => {
                info!("✅ Размещено {} ордеров через post_orders", responses.len());

                // Обрабатываем каждый ответ
                for (idx, response) in responses.iter().enumerate() {
                    let is_up = idx == 0; // Первый ордер - UP, второй - DOWN
                    let price = if is_up { up_rounded } else { down_rounded };

                    if !response.order_id.is_empty() {
                        info!("📝 GTC лимитка размещена: {} @ {:.3} | order_id={}",
                            if is_up { "UP" } else { "DOWN" }, price, response.order_id);

                        // Записываем order_id в список активных
                        engine_clone.add_order_id(response.order_id.clone());
                    } else {
                        warn!("⚠️ Ордер {} размещен но order_id пустой", if is_up { "UP" } else { "DOWN" });
                    }
                }

                // Если хотя бы один ордер не размещен - уменьшаем счетчик
                if responses.is_empty() || responses.iter().any(|r| r.order_id.is_empty()) {
                    engine_clone.decrement_orders_count();
                }
            },
            Err(e) => {
                warn!("❌ Ошибка размещения ордеров через post_orders: {}", e);
                // Уменьшаем счетчик, так как ордера не были размещены
                engine_clone.decrement_orders_count();
            },
        }
    });
}

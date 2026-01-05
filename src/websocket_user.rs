use futures::StreamExt;
use std::sync::Arc;
use std::time::Duration;
use polymarket_client_sdk::clob::ws::{Client, WsMessage};
use polymarket_client_sdk::auth::state::Authenticated;
use polymarket_client_sdk::auth::Normal;
use crate::engine::RealEngine;
use tracing::{info, warn};

pub struct UserStream {
    engine: Arc<RealEngine>,
    // ИСПРАВЛЕНИЕ: Указываем точный тип аутентифицированного клиента
    client: Client<Authenticated<Normal>>, 
}

impl UserStream {
    pub fn new(engine: Arc<RealEngine>, client: Client<Authenticated<Normal>>) -> Self {
        Self { engine, client }
    }

    pub async fn start_stream(&self) -> anyhow::Result<()> {
        let mut reconnect_delay = Duration::from_secs(1);
        const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(60);

        loop {
            match self.run_stream_once().await {
                Ok(_) => {
                    info!("👤 User Stream завершен нормально");
                    return Ok(());
                }
                Err(e) => {
                    warn!("👤 User WS отключен: {}. Переподключение через {:?}...", e, reconnect_delay);
                    tokio::time::sleep(reconnect_delay).await;

                    reconnect_delay = (reconnect_delay * 2).min(MAX_RECONNECT_DELAY);
                }
            }
        }
    }

    async fn run_stream_once(&self) -> anyhow::Result<()> {
        info!("👂 Подписываемся на User Events...");

        let markets: Vec<String> = Vec::new();

        let mut stream = std::pin::pin!(self.client.subscribe_user_events(markets)?);

        info!("✅ User Stream подключен");

        while let Some(event) = stream.next().await {
            match event {
                Ok(WsMessage::Trade(trade)) => {
                    use rust_decimal::prelude::ToPrimitive;

                    let trade_id = trade.id.clone();
                    let price: f64 = trade.price.to_f64().unwrap_or(0.0);
                    let size: f64 = trade.size.to_f64().unwrap_or(0.0);
                    let asset_id = trade.asset_id.to_string();

                    self.engine.handle_ws_trade(trade_id, price, size, trade.side, &asset_id);
                }
                Ok(WsMessage::Order(order)) => {
                    use rust_decimal::prelude::ToPrimitive;

                    let order_id = order.id.clone();
                    let msg_type = order.msg_type.clone();
                    let price: f64 = order.price.to_f64().unwrap_or(0.0);
                    let asset_id = order.asset_id.to_string();

                    // size_matched показывает сколько было исполнено (для UPDATE событий)
                    let size_matched: Option<f64> = order.size_matched
                        .and_then(|d| d.to_f64());

                    self.engine.handle_ws_order(order_id, msg_type, price, order.side, &asset_id, size_matched);
                }
                Ok(_) => {}
                Err(e) => {
                    return Err(anyhow::anyhow!("Ошибка в User Stream: {}", e));
                }
            }
        }

        Ok(())
    }
}
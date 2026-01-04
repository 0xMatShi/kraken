use futures::StreamExt;
use std::sync::Arc;
// Импортируем правильные типы для клиента
use polymarket_client_sdk::clob::ws::{Client, WsMessage};
use polymarket_client_sdk::auth::state::Authenticated;
use polymarket_client_sdk::auth::Normal;
use crate::engine::RealEngine;

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
        println!("👂 Подписываемся на User Events...");

        let markets: Vec<String> = Vec::new(); 
        
        // Метод subscribe_user_events доступен только у Authenticated клиента
        let mut stream = std::pin::pin!(self.client.subscribe_user_events(markets)?);

        println!("✅ User Stream запущен. Ждем обновлений...");

        while let Some(event) = stream.next().await {
            match event {
                Ok(WsMessage::Trade(trade)) => {
                    // ИСПРАВЛЕНИЕ: Преобразуем данные здесь, чтобы не тащить типы в engine
                    let price: f64 = trade.price.to_string().parse().unwrap_or(0.0);
                    let size: f64 = trade.size.to_string().parse().unwrap_or(0.0);
                    let asset_id = trade.asset_id.to_string(); // Возможно trade.asset_id уже String, тогда .clone()

                    println!("⚡ WS TRADE: ID {} | Size {} | Price {}", trade.id, size, price);
                    
                    self.engine.handle_ws_trade(price, size, trade.side, &asset_id);
                }
                Ok(WsMessage::Order(_order)) => {
                    // Можно раскомментировать для отладки
                    // println!("📋 WS ORDER: ID {} Status: {:?}", _order.id, _order.msg_type);
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("❌ Ошибка в User Stream: {}", e);
                    break;
                }
            }
        }
        
        Ok(())
    }
}
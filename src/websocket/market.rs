use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use crate::models::{BookMessage, SubscribeMessage, MarketPrices};
use crate::trading::engine::RealEngine;
use std::sync::{Arc, Mutex};
use chrono::{DateTime, Utc};
use tokio::time::{interval, Duration};
use tracing::{info, warn};


pub struct DataStream {
    up_token: String,
    down_token: String,
    ws_url: String,
    prices: Arc<Mutex<MarketPrices>>,
    engine: Arc<RealEngine>,
}

impl DataStream {
    pub fn new(up: String, down: String, engine: Arc<RealEngine>, ws_url: String) -> Self {
        Self {
            up_token: up,
            down_token: down,
            ws_url,
            prices: Arc::new(Mutex::new(MarketPrices::default())),
            engine,
        }
    }

    pub async fn start_stream(&self, end_date_str: String) -> Result<(), Box<dyn std::error::Error>> {
        let end_date = end_date_str.parse::<DateTime<Utc>>().unwrap_or(Utc::now());
        let mut reconnect_delay = Duration::from_secs(1);
        const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(60);

        loop {
            if Utc::now() >= end_date {
                let last_p = *self.prices.lock().unwrap();
                self.engine.finalize(&last_p);
                return Ok(());
            }

            match self.run_stream_once(end_date).await {
                Ok(_) => {
                    let last_p = *self.prices.lock().unwrap();
                    self.engine.finalize(&last_p);
                    return Ok(());
                }
                Err(e) => {
                    warn!("📉 Market WS отключен: {}. Переподключение через {:?}...", e, reconnect_delay);
                    tokio::time::sleep(reconnect_delay).await;

                    reconnect_delay = (reconnect_delay * 2).min(MAX_RECONNECT_DELAY);
                }
            }
        }
    }

    async fn run_stream_once(&self, end_date: DateTime<Utc>) -> Result<(), Box<dyn std::error::Error>> {
        let (mut ws_stream, _) = connect_async(&self.ws_url).await?;

        let sub = SubscribeMessage {
            assets_ids: vec![self.up_token.clone(), self.down_token.clone()],
            msg_type: "market".to_string(),
        };
        ws_stream.send(Message::Text(serde_json::to_string(&sub)?.into())).await?;

        info!("✅ Market WS подключен");

        let mut check_interval = interval(Duration::from_secs(1));

        loop {
            tokio::select! {
                msg = ws_stream.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            if let Ok(book) = serde_json::from_str::<BookMessage>(&text) {
                                self.update_prices(book);
                            }
                        }
                        Some(Ok(Message::Close(_))) => {
                            return Err("WebSocket закрыт сервером".into());
                        }
                        Some(Err(e)) => {
                            return Err(Box::new(e));
                        }
                        None => {
                            return Err("Соединение потеряно".into());
                        }
                        _ => {}
                    }
                }
                _ = check_interval.tick() => {
                    if Utc::now() >= end_date {
                        return Ok(());
                    }
                }
            }
        }
    }

    fn update_prices(&self, book: BookMessage) {
        let mut p = self.prices.lock().unwrap();

        let best_bid = book.bids.iter()
            .map(|o| (o.price, o.size))
            .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        let best_ask = book.asks.iter()
            .map(|o| (o.price, o.size))
            .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap());

        if book.asset_id == self.up_token {
            if let Some(b) = best_bid { p.up_bid = b.0; p.up_bid_size = b.1; }
            if let Some(a) = best_ask { p.up_ask = a.0; p.up_ask_size = a.1; }
        } else {
            if let Some(b) = best_bid { p.down_bid = b.0; p.down_bid_size = b.1; }
            if let Some(a) = best_ask { p.down_ask = a.0; p.down_ask_size = a.1; }
        }

        // ВАЖНО: Копируем без аллокации (MarketPrices теперь Copy)
        self.engine.process_tick(*p);
    }
}
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use crate::models::{BookMessage, SubscribeMessage, MarketPrices};
use crate::engine::RealEngine;
use std::sync::{Arc, Mutex};
use chrono::{DateTime, Utc};
use tokio::time::{interval, Duration};

pub struct DataStream {
    up_token: String,
    down_token: String,
    ws_url: String,
    prices: Arc<Mutex<MarketPrices>>,
    engine: Arc<RealEngine>,
}

impl DataStream {
    pub fn new(up: String, down: String, engine: Arc<RealEngine>) -> Self {
        Self {
            up_token: up,
            down_token: down,
            ws_url: "wss://ws-subscriptions-clob.polymarket.com/ws/market".to_string(),
            prices: Arc::new(Mutex::new(MarketPrices::default())),
            engine,
        }
    }

    pub async fn start_stream(&self, end_date_str: String) -> Result<(), Box<dyn std::error::Error>> {
        let (mut ws_stream, _) = connect_async(&self.ws_url).await?;
        
        let sub = SubscribeMessage {
            assets_ids: vec![self.up_token.clone(), self.down_token.clone()],
            msg_type: "market".to_string(),
        };
        ws_stream.send(Message::Text(serde_json::to_string(&sub)?.into())).await?;

        let end_date = end_date_str.parse::<DateTime<Utc>>().unwrap_or(Utc::now());
        let mut check_interval = interval(Duration::from_secs(1));

        loop {
            tokio::select! {
                msg = ws_stream.next() => {
                    if let Some(Ok(Message::Text(text))) = msg {
                        if let Ok(book) = serde_json::from_str::<BookMessage>(&text) {
                            self.update_prices(book);
                        }
                    } else if msg.is_none() { break; }
                }
                _ = check_interval.tick() => {
                    if Utc::now() >= end_date { break; }
                }
            }
        }
        
        let last_p = self.prices.lock().unwrap().clone();
        self.engine.finalize(&last_p);
        Ok(())
    }

    fn update_prices(&self, book: BookMessage) {
        let mut p = self.prices.lock().unwrap();
        
        let best_bid = book.bids.iter().filter_map(|o| Some((o.price.parse::<f64>().ok()?, o.size.parse::<f64>().ok()?)))
            .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        let best_ask = book.asks.iter().filter_map(|o| Some((o.price.parse::<f64>().ok()?, o.size.parse::<f64>().ok()?)))
            .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap());

        if book.asset_id == self.up_token {
            if let Some(b) = best_bid { p.up_bid = b.0; p.up_bid_size = b.1; }
            if let Some(a) = best_ask { p.up_ask = a.0; p.up_ask_size = a.1; }
        } else {
            if let Some(b) = best_bid { p.down_bid = b.0; p.down_bid_size = b.1; }
            if let Some(a) = best_ask { p.down_ask = a.0; p.down_ask_size = a.1; }
        }
        
        // ВАЖНО: Вызываем движок БЕЗ задержки
        self.engine.process_tick(p.clone());
    }
}
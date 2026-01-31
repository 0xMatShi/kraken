use crate::core::RealEngine;
use crate::models::{BookMessage, MarketPrices, SubscribeMessage};
use crate::ui::{self, ORDER_BOOK_DEPTH, OrderLevel, UiState};
use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use std::sync::{Arc, Mutex};
use tokio::time::{Duration, interval};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tracing::{info, warn};

pub struct DataStream {
    up_token: String,
    down_token: String,
    ws_url: String,
    prices: Arc<Mutex<MarketPrices>>,
    engine: Arc<RealEngine>,
    ui_state: UiState,
}

impl DataStream {
    pub fn new(
        up: String,
        down: String,
        engine: Arc<RealEngine>,
        ws_url: String,
        ui_state: UiState,
    ) -> Self {
        Self {
            up_token: up,
            down_token: down,
            ws_url,
            prices: Arc::new(Mutex::new(MarketPrices::default())),
            engine,
            ui_state,
        }
    }

    pub async fn start_stream(
        &self,
        end_date_str: String,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let end_date = end_date_str.parse::<DateTime<Utc>>().unwrap_or(Utc::now());

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
                    warn!(
                        "📉 Market WS отключен: {}. Моментальное переподключение...",
                        e
                    );
                    // Моментальное переподключение без задержки
                }
            }
        }
    }

    async fn run_stream_once(
        &self,
        end_date: DateTime<Utc>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (mut ws_stream, _) = connect_async(&self.ws_url).await?;

        let sub = SubscribeMessage {
            assets_ids: vec![self.up_token.clone(), self.down_token.clone()],
            msg_type: "market".to_string(),
        };
        ws_stream
            .send(Message::Text(serde_json::to_string(&sub)?.into()))
            .await?;

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

        // Сортируем bids по убыванию цены, asks по возрастанию
        let mut sorted_bids: Vec<_> = book.bids.iter().map(|o| (o.price, o.size)).collect();
        sorted_bids.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        let mut sorted_asks: Vec<_> = book.asks.iter().map(|o| (o.price, o.size)).collect();
        sorted_asks.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

        // Извлекаем уровни для UI
        let mut ui_bids = [OrderLevel::default(); ORDER_BOOK_DEPTH];
        let mut ui_asks = [OrderLevel::default(); ORDER_BOOK_DEPTH];

        for (i, (price, size)) in sorted_bids.iter().take(ORDER_BOOK_DEPTH).enumerate() {
            ui_bids[i] = OrderLevel {
                price: *price,
                size: *size,
            };
        }
        for (i, (price, size)) in sorted_asks.iter().take(ORDER_BOOK_DEPTH).enumerate() {
            ui_asks[i] = OrderLevel {
                price: *price,
                size: *size,
            };
        }

        // Обновляем MarketPrices (лучший bid/ask и второй уровень bid)
        let best_bid = sorted_bids.first().copied();
        let second_bid = sorted_bids.get(1).copied();
        let best_ask = sorted_asks.first().copied();

        if book.asset_id == self.up_token {
            if let Some(b) = best_bid {
                p.up_bid = b.0;
                p.up_bid_size = b.1;
            }
            if let Some(b2) = second_bid {
                p.up_bid_2 = b2.0;
                p.up_bid_size_2 = b2.1;
            }
            if let Some(a) = best_ask {
                p.up_ask = a.0;
                p.up_ask_size = a.1;
            }
            // Обновляем UI state для UP стакана
            ui::update_up_book(&self.ui_state, ui_bids, ui_asks);
        } else {
            if let Some(b) = best_bid {
                p.down_bid = b.0;
                p.down_bid_size = b.1;
            }
            if let Some(b2) = second_bid {
                p.down_bid_2 = b2.0;
                p.down_bid_size_2 = b2.1;
            }
            if let Some(a) = best_ask {
                p.down_ask = a.0;
                p.down_ask_size = a.1;
            }
            // Обновляем UI state для DOWN стакана
            ui::update_down_book(&self.ui_state, ui_bids, ui_asks);
        }

        // ВАЖНО: Копируем без аллокации (MarketPrices теперь Copy)
        self.engine.process_tick(*p);
    }
}

use crate::core::RealEngine;
use crate::models::{BookMessage, MarketPrices, PriceChange, PriceChangeMessage, SubscribeMessage};
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
                            // Пробуем распарсить как BookMessage
                            if let Ok(book) = serde_json::from_str::<BookMessage>(&text) {
                                self.update_prices(book);
                            }
                            // Пробуем распарсить как PriceChangeMessage
                            else if let Ok(price_change) = serde_json::from_str::<PriceChangeMessage>(&text) {
                                self.update_prices_from_change(price_change);
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

    /// Обновляет цены на основе price_change события
    fn update_prices_from_change(&self, msg: PriceChangeMessage) {
        // Обрабатываем каждое изменение и группируем по asset_id
        let mut up_changes = Vec::new();
        let mut down_changes = Vec::new();

        for change in msg.price_changes {
            if change.asset_id == self.up_token {
                up_changes.push(change);
            } else {
                down_changes.push(change);
            }
        }

        // Обрабатываем изменения для UP токена
        if !up_changes.is_empty() {
            self.apply_changes_to_book(&up_changes, true);
        }

        // Обрабатываем изменения для DOWN токена
        if !down_changes.is_empty() {
            self.apply_changes_to_book(&down_changes, false);
        }

        // Обновляем MarketPrices и триггерим process_tick один раз
        let p = *self.prices.lock().unwrap();
        self.engine.process_tick(p);
    }

    /// Применяет изменения цен к orderbook и обновляет MarketPrices
    fn apply_changes_to_book(&self, changes: &[PriceChange], is_up: bool) {
        // Получаем текущий orderbook из UI
        let (mut current_bids, mut current_asks) = if is_up {
            ui::get_up_book(&self.ui_state)
        } else {
            ui::get_down_book(&self.ui_state)
        };

        // Применяем каждое изменение к соответствующему уровню
        for change in changes {
            let is_buy = change.side == "BUY";
            let levels = if is_buy {
                &mut current_bids
            } else {
                &mut current_asks
            };

            // Ищем уровень с этой ценой
            let mut found = false;
            for level in levels.iter_mut() {
                if (level.price - change.price).abs() < 0.001 {
                    // Нашли уровень - обновляем размер
                    if change.size == 0.0 {
                        // Размер 0 = удаляем уровень
                        level.price = 0.0;
                        level.size = 0.0;
                    } else {
                        level.size = change.size;
                    }
                    found = true;
                    break;
                }
            }

            // Если уровень не найден и size > 0, добавляем новый
            if !found && change.size > 0.0 {
                // Ищем пустой слот или слот с нулевым размером
                for level in levels.iter_mut() {
                    if level.size == 0.0 {
                        level.price = change.price;
                        level.size = change.size;
                        break;
                    }
                }
            }
        }

        // Убираем уровни с нулевым размером и сортируем
        let mut active_bids: Vec<_> = current_bids
            .iter()
            .filter(|l| l.size > 0.0)
            .copied()
            .collect();
        let mut active_asks: Vec<_> = current_asks
            .iter()
            .filter(|l| l.size > 0.0)
            .copied()
            .collect();

        // Сортируем: bids по убыванию, asks по возрастанию
        active_bids.sort_by(|a, b| {
            b.price
                .partial_cmp(&a.price)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        active_asks.sort_by(|a, b| {
            a.price
                .partial_cmp(&b.price)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Берем top ORDER_BOOK_DEPTH уровней
        let mut ui_bids = [OrderLevel::default(); ORDER_BOOK_DEPTH];
        let mut ui_asks = [OrderLevel::default(); ORDER_BOOK_DEPTH];

        for (i, level) in active_bids.iter().take(ORDER_BOOK_DEPTH).enumerate() {
            ui_bids[i] = *level;
        }
        for (i, level) in active_asks.iter().take(ORDER_BOOK_DEPTH).enumerate() {
            ui_asks[i] = *level;
        }

        // Обновляем UI
        if is_up {
            ui::update_up_book(&self.ui_state, ui_bids, ui_asks);
        } else {
            ui::update_down_book(&self.ui_state, ui_bids, ui_asks);
        }

        // Обновляем MarketPrices на основе изменений
        // Используем best_bid/best_ask из последнего изменения (они одинаковые для всех изменений в одном сообщении)
        if let Some(last_change) = changes.last() {
            let mut p = self.prices.lock().unwrap();
            if is_up {
                p.up_bid = last_change.best_bid;
                p.up_ask = last_change.best_ask;
                // Обновляем размеры из orderbook
                if let Some(best_bid_level) = ui_bids.first() {
                    if (best_bid_level.price - last_change.best_bid).abs() < 0.001 {
                        p.up_bid_size = best_bid_level.size;
                    }
                }
                if let Some(second_bid_level) = ui_bids.get(1) {
                    if second_bid_level.size > 0.0 {
                        p.up_bid_2 = second_bid_level.price;
                        p.up_bid_size_2 = second_bid_level.size;
                    }
                }
                if let Some(best_ask_level) = ui_asks.first() {
                    if (best_ask_level.price - last_change.best_ask).abs() < 0.001 {
                        p.up_ask_size = best_ask_level.size;
                    }
                }
            } else {
                p.down_bid = last_change.best_bid;
                p.down_ask = last_change.best_ask;
                // Обновляем размеры из orderbook
                if let Some(best_bid_level) = ui_bids.first() {
                    if (best_bid_level.price - last_change.best_bid).abs() < 0.001 {
                        p.down_bid_size = best_bid_level.size;
                    }
                }
                if let Some(second_bid_level) = ui_bids.get(1) {
                    if second_bid_level.size > 0.0 {
                        p.down_bid_2 = second_bid_level.price;
                        p.down_bid_size_2 = second_bid_level.size;
                    }
                }
                if let Some(best_ask_level) = ui_asks.first() {
                    if (best_ask_level.price - last_change.best_ask).abs() < 0.001 {
                        p.down_ask_size = best_ask_level.size;
                    }
                }
            }
        }
    }
}

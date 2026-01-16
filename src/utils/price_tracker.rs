use std::collections::HashMap;
use std::fs;
use std::path::Path;
use serde::{Deserialize, Serialize};
use crate::models::Coin;

const PRICE_TRACKER_FILE: &str = "price_tracker.json";

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PriceEntry {
    pub price: f64,
    pub timestamp: i64,  // Unix timestamp окончания события
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct PriceTracker {
    prices: HashMap<String, PriceEntry>,  // Key: "BTC", "ETH", etc.
}

impl PriceTracker {
    pub fn load() -> Self {
        if Path::new(PRICE_TRACKER_FILE).exists() {
            if let Ok(contents) = fs::read_to_string(PRICE_TRACKER_FILE) {
                if let Ok(tracker) = serde_json::from_str(&contents) {
                    return tracker;
                }
            }
        }
        Self::default()
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        fs::write(PRICE_TRACKER_FILE, json)?;
        Ok(())
    }

    /// Получить "price to beat" для следующего события
    /// Возвращает Some(price) если предыдущее событие было ровно перед этим (timestamp предыдущего + 900)
    /// Возвращает None если нет данных или событие не следующее
    pub fn get_price_to_beat(&self, coin: Coin, next_event_timestamp: i64) -> Option<f64> {
        let coin_key = format!("{:?}", coin);

        if let Some(entry) = self.prices.get(&coin_key) {
            // Проверяем, что следующее событие ровно через 900 секунд
            if entry.timestamp + 900 == next_event_timestamp {
                return Some(entry.price);
            }
        }
        None
    }

    /// Сохранить последнюю цену для монеты
    pub fn set_last_price(&mut self, coin: Coin, price: f64, event_timestamp: i64) {
        let coin_key = format!("{:?}", coin);
        self.prices.insert(coin_key, PriceEntry {
            price,
            timestamp: event_timestamp,
        });
    }
}

/// Извлечь timestamp из slug события
/// Пример: "btc-updown-15m-1768090500" -> Some(1768090500)
pub fn extract_timestamp_from_slug(slug: &str) -> Option<i64> {
    slug.split('-')
        .last()
        .and_then(|s| s.parse::<i64>().ok())
}

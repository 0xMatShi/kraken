use reqwest::Client;
use chrono::{DateTime, Utc};
use tokio::time::{sleep, Duration};
use crate::models::{PolymarketEvent, TargetMarket, Market};
use std::error::Error;

pub struct AutoScanner {
    client: Client,
    api_url: String,
}

impl AutoScanner {
    pub fn new() -> Self {
        Self {
            client: Client::new(),
            api_url: "https://gamma-api.polymarket.com/events".to_string(),
        }
    }

    pub async fn find_next_target(
        &self, 
        target_prefix: &str, 
        min_m: f64, 
        max_m: f64
    ) -> Option<TargetMarket> {
        println!("🤖 Авто-поиск {} (окно: {}-{} мин)", target_prefix, min_m, max_m);

        loop {
            // Пытаемся выполнить одну итерацию поиска
            match self.perform_scan(target_prefix, min_m, max_m).await {
                Ok(Some(target)) => return Some(target),
                Ok(None) => (), // Ничего не нашли, продолжаем цикл
                Err(e) => eprintln!("⚠️ Ошибка при сканировании: {}", e),
            }

            sleep(Duration::from_secs(5)).await;
        }
    }

    // Выносим логику одного запроса в отдельный метод
    async fn perform_scan(&self, prefix: &str, min_m: f64, max_m: f64) -> Result<Option<TargetMarket>, Box<dyn Error>> {
        let mut offset = 0;
        let limit = 500;

        loop {
            let start_request = std::time::Instant::now();
            
            // Формируем запрос с учетом текущего offset
            let response = self.client.get(&self.api_url)
                .query(&[
                    ("active", "true"), 
                    ("closed", "false"), 
                    ("limit", &limit.to_string()), // Лимит за раз
                    ("offset", &offset.to_string()), // Наше смещение
                    ("order", "endDate"), 
                    ("ascending", "true")
                ])
                .send()
                .await?;

            // Проверяем статус ответа (важно для отладки)
            if !response.status().is_success() {
                return Err(format!("API вернул ошибку: {}", response.status()).into());
            }

            let bytes = response.bytes().await?;
            let download_time = start_request.elapsed();
            
            let start_parse = std::time::Instant::now();
            let events: Vec<PolymarketEvent> = serde_json::from_slice(&bytes)?;
            
            // Если API вернул пустой список, значит мы просмотрели всё и ничего не нашли
            if events.is_empty() {
                println!("📍 Достигнут конец списка событий. Ничего не найдено.");
                return Ok(None);
            }

            println!(
                "📡 Загружено {} событий (offset: {}). Сеть: {:?} | Парсинг: {:?}", 
                events.len(), offset, download_time, start_parse.elapsed()
            );

            // Ищем цель в текущей пачке
            let target = events.into_iter()
                .filter(|e| e.active && e.slug.starts_with(prefix))
                .filter_map(|e| self.process_event(e, min_m, max_m))
                .next();

            // Если нашли — возвращаем результат немедленно
            if target.is_some() {
                return Ok(target);
            }

            // Если не нашли в этой пачке — увеличиваем offset и идем на следующий круг
            offset += limit;
            
            // Небольшая пауза между запросами пагинации, чтобы API не забанил за спам
            // (Rate limiting — важная штука в арбитраже)
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    // Логика проверки конкретного события
    fn process_event(&self, event: PolymarketEvent, min_m: f64, max_m: f64) -> Option<TargetMarket> {
        let end_dt = event.end_date.parse::<DateTime<Utc>>().ok()?;
        let minutes_left = end_dt.signed_duration_since(Utc::now()).num_seconds() as f64 / 60.0;

        // Проверка окна времени
        if minutes_left <= min_m || minutes_left > max_m {
            return None;
        }

        // Пытаемся достать токены из рынков
        let markets: Vec<Market> = serde_json::from_value(event.markets).ok()?;
        
        for market in markets {
            let tokens: Vec<String> = serde_json::from_str(&market.clob_token_ids).ok()?;
            if tokens.len() >= 2 {
                println!("🔎 НАЙДЕНО: {} ({:.1} мин)", event.slug, minutes_left);
                return Some(TargetMarket {
                    slug: event.slug,
                    title: event.title,
                    up_token: tokens[0].clone(),
                    down_token: tokens[1].clone(),
                    end_date: event.end_date,
                });
            }
        }

        None
    }
}
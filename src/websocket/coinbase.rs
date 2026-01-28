use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use futures_util::{StreamExt, SinkExt};
use serde_json::json;
use crate::models::Coin;
use crate::ui::UiState;
use tracing::{info, warn};

const COINBASE_WS_URL: &str = "wss://ws-feed.exchange.coinbase.com";

pub struct CoinbaseStream {
    coin: Coin,
    ui_state: UiState,
}

impl CoinbaseStream {
    pub fn new(coin: Coin, ui_state: UiState) -> Self {
        Self { coin, ui_state }
    }

    pub async fn start_stream(self) -> anyhow::Result<()> {
        loop {
            match self.run_stream_once().await {
                Ok(_) => {
                    info!("💰 Coinbase Stream завершен нормально");
                    return Ok(());
                }
                Err(e) => {
                    warn!("💰 Coinbase WS отключен: {}. Моментальное переподключение...", e);
                    // Моментальное переподключение без задержки
                }
            }
        }
    }

    async fn run_stream_once(&self) -> anyhow::Result<()> {
        let (ws_stream, _) = connect_async(COINBASE_WS_URL).await?;
        info!("✅ Соединение с Coinbase установлено");

        let (mut write, mut read) = ws_stream.split();

        // Подписываемся на тикер для выбранной монеты
        let subscribe_msg = json!({
            "type": "subscribe",
            "product_ids": [self.coin.coinbase_product()],
            "channels": ["ticker"]
        });

        write.send(Message::Text(subscribe_msg.to_string().into())).await?;
        info!("📡 Подписка на {} активна", self.coin.coinbase_product());

        while let Some(msg) = read.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    if let Ok(data) = serde_json::from_str::<serde_json::Value>(&text) {
                        if data.get("type").and_then(|t| t.as_str()) == Some("ticker") {
                            if let Some(price_str) = data.get("price").and_then(|p| p.as_str()) {
                                if let Ok(price) = price_str.parse::<f64>() {
                                    // Обновляем текущую цену в UI state
                                    crate::ui::set_current_price(&self.ui_state, price);
                                }
                            }
                        }
                    }
                }
                Ok(Message::Close(_)) => {
                    return Err(anyhow::anyhow!("Coinbase WebSocket закрыт сервером"));
                }
                Err(e) => {
                    return Err(anyhow::anyhow!("Ошибка Coinbase WebSocket: {}", e));
                }
                _ => {}
            }
        }

        // Стрим завершился без ошибки - соединение потеряно
        Err(anyhow::anyhow!("Coinbase соединение потеряно"))
    }
}

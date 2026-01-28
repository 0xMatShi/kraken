use anyhow::{Context, Result};
use serde::Deserialize;
use std::fs;

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub trading: TradingConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct TradingConfig {
    /// Максимальный баланс для торговли (USD)
    pub max_balance: f64,

    /// Размер одного ордера (количество акций)
    pub size: f64,

    /// Через сколько секунд после начала события можно начинать торговать
    #[serde(default = "default_seconds_before_start")]
    pub seconds_before_start: i64,
}

fn default_seconds_before_start() -> i64 {
    20
}

impl Config {
    /// Загружает конфигурацию из файла config.toml
    pub fn load() -> Result<Self> {
        let config_path = "config.toml";
        let config_str = fs::read_to_string(config_path)
            .with_context(|| format!("Не удалось прочитать файл {}", config_path))?;

        let config: Config = toml::from_str(&config_str)
            .with_context(|| format!("Не удалось распарсить {}", config_path))?;

        Ok(config)
    }
}

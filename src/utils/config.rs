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

    /// Виртуальный лимит для cheap side (максимум акций)
    #[serde(default = "default_cheap_limit")]
    pub cheap_limit: f64,

    /// Время экспирации GTD ордеров (секунды)
    #[serde(default = "default_expiration_seconds")]
    pub expiration_seconds: u64,
}

fn default_cheap_limit() -> f64 {
    150.0
}

fn default_expiration_seconds() -> u64 {
    2
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

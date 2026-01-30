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

    /// Стратегия размещения первых ног: "strong", "weak", "both"
    #[serde(default = "default_legs_strategy")]
    pub legs_strategy: String,
}

fn default_seconds_before_start() -> i64 {
    20
}

fn default_legs_strategy() -> String {
    "both".to_string()
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

    /// Сохраняет конфигурацию в файл config.toml
    pub fn save(&self) -> Result<()> {
        let config_path = "config.toml";

        // Форматируем f64 значения явно с одним знаком после запятой минимум
        let max_balance_str = if self.trading.max_balance.fract() == 0.0 {
            format!("{:.1}", self.trading.max_balance)
        } else {
            format!("{}", self.trading.max_balance)
        };

        let size_str = if self.trading.size.fract() == 0.0 {
            format!("{:.1}", self.trading.size)
        } else {
            format!("{}", self.trading.size)
        };

        let toml_content = format!(
r#"# Торговые параметры MMDNA бота

[trading]
# Максимальный баланс для торговли (USD)
max_balance = {}

# Размер одного ордера (количество акций)
size = {}

# Через сколько секунд после начала события можно начинать торговать
seconds_before_start = {}

# Стратегия размещения первых ног: "strong" (только на сильной), "weak" (только на слабой), "both" (обе)
legs_strategy = "{}"
"#,
            max_balance_str,
            size_str,
            self.trading.seconds_before_start,
            self.trading.legs_strategy
        );

        fs::write(config_path, toml_content)
            .with_context(|| format!("Не удалось записать в файл {}", config_path))?;

        Ok(())
    }
}

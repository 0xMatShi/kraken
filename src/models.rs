use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Debug, Deserialize, Clone)]
pub struct Market {
    #[serde(rename = "clobTokenIds")]
    pub clob_token_ids: String,
    #[serde(rename = "conditionId")]
    pub condition_id: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PolymarketEvent {
    pub slug: String,
    pub title: String,
    pub end_date: String,
    pub active: bool,
    pub markets: serde_json::Value,
}

#[allow(dead_code)]
pub struct TargetMarket {
    pub slug: String,
    pub title: String,
    pub up_token: String,
    pub down_token: String,
    pub end_date: String,
    pub condition_id: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct OrderSummary {
    #[serde(deserialize_with = "deserialize_f64_from_string")]
    pub price: f64,
    #[serde(deserialize_with = "deserialize_f64_from_string")]
    pub size: f64,
}

fn deserialize_f64_from_string<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let s = String::deserialize(deserializer)?;
    s.parse::<f64>().map_err(D::Error::custom)
}

#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
pub struct BookMessage {
    pub event_type: String,
    pub asset_id: String,
    pub bids: Vec<OrderSummary>,
    pub asks: Vec<OrderSummary>,
}

#[derive(Debug, Serialize)]
pub struct SubscribeMessage {
    pub assets_ids: Vec<String>,
    #[serde(rename = "type")]
    pub msg_type: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Side {
    Up,
    Down,
}

impl Side {
    pub fn opposite(&self) -> Side {
        match self {
            Side::Up => Side::Down,
            Side::Down => Side::Up,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Coin {
    BTC,
    ETH,
    SOL,
    XRP,
}

impl Coin {
    pub fn slug_prefix(&self) -> &'static str {
        match self {
            Coin::BTC => "btc-updown-15m",
            Coin::ETH => "eth-updown-15m",
            Coin::SOL => "sol-updown-15m",
            Coin::XRP => "xrp-updown-15m",
        }
    }

    pub fn from_index(index: u8) -> Option<Self> {
        match index {
            1 => Some(Coin::BTC),
            2 => Some(Coin::ETH),
            3 => Some(Coin::SOL),
            4 => Some(Coin::XRP),
            _ => None,
        }
    }
}

/// Тренд рынка
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Trend {
    /// Тренд сильной стороны: bb слабой стороны уменьшился
    Strong,
    /// Тренд слабой стороны: bb сильной стороны уменьшился
    Weak,
    /// Нет тренда (спред нормальный или цены не изменились)
    None,
}

/// Фаза cumulative стратегии
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CumulativePhase {
    AccumulatingFirstLeg,
    PlacingSecondLeg,
}

/// Состояние cumulative стратегии
#[derive(Debug)]
pub struct CumulativeState {
    pub phase: CumulativePhase,
    pub first_leg_side: Side,
    pub first_leg_filled: f64,
    pub first_leg_orders: HashSet<String>,
    pub first_leg_placed_price: Option<f64>, // предотвращает дублирование размещения на одной цене
    pub second_leg_filled: f64,
    pub second_leg_orders: HashSet<String>,
    pub second_leg_placed_price: Option<f64>,
}

/// Состояние первой ноги торговой пары
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct FirstLeg {
    pub order_id: String,
    pub price: f64,  // Цена размещения (в центах, например 0.65)
    pub size: f64,   // Размер ордера
    pub side: Side,  // На какой стороне размещена (Up или Down)
    pub filled: f64, // Сколько исполнено
}

/// Состояние второй ноги торговой пары
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SecondLeg {
    pub order_id: String,
    pub price: f64,           // Цена размещения = 0.99 - first_leg_price
    pub size: f64,            // Размер = размер первой ноги
    pub side: Side,           // Противоположная сторона от первой ноги
    pub filled: f64,          // Сколько исполнено
    pub first_leg_price: f64, // Цена первой ноги (для блокировки)
    pub last_placed: std::time::Instant, // Время последнего размещения/переразмещения
    pub timer_interval_secs: u64, // Интервал таймера переразмещения (9, 8, 7, ... 1)
}

/// Полная торговая пара (первая нога + вторая нога)
#[derive(Debug, Clone)]
pub struct TradePair {
    pub first_leg: FirstLeg,
    pub second_leg: Option<SecondLeg>, // None пока первая нога не исполнена
}

/// Система блокировки цен
/// Хранит цены (в центах как u32) на которых размещены первые ноги
/// Цена освобождается только когда вторая нога полностью исполнена
#[derive(Debug, Default)]
pub struct PriceLock {
    /// Заблокированные цены на UP стороне
    pub up_locked: HashSet<u32>,
    /// Заблокированные цены на DOWN стороне  
    pub down_locked: HashSet<u32>,
}

impl PriceLock {
    /// Проверяет, заблокирована ли цена на данной стороне
    pub fn is_locked(&self, side: Side, price: f64) -> bool {
        let price_cents = (price * 100.0).round() as u32;
        match side {
            Side::Up => self.up_locked.contains(&price_cents),
            Side::Down => self.down_locked.contains(&price_cents),
        }
    }

    /// Блокирует цену
    pub fn lock(&mut self, side: Side, price: f64) {
        let price_cents = (price * 100.0).round() as u32;
        match side {
            Side::Up => self.up_locked.insert(price_cents),
            Side::Down => self.down_locked.insert(price_cents),
        };
    }

    /// Разблокирует цену
    pub fn unlock(&mut self, side: Side, price: f64) {
        let price_cents = (price * 100.0).round() as u32;
        match side {
            Side::Up => self.up_locked.remove(&price_cents),
            Side::Down => self.down_locked.remove(&price_cents),
        };
    }
}

#[derive(Debug, Default, Clone)]
pub struct Portfolio {
    pub up_shares: f64,
    pub down_shares: f64,
    pub up_spent: f64,
    pub down_spent: f64,
    pub maker_trades: u32,
    pub taker_trades: u32,
    // Отслеживание выставленных лимиток
    pub up_total_placed: f64,   // Всего shares выставлено в UP лимитках
    pub down_total_placed: f64, // Всего shares выставлено в DOWN лимитках
}

#[allow(dead_code)]
impl Portfolio {
    pub fn up_avg(&self) -> f64 {
        if self.up_shares > 0.0 {
            self.up_spent / self.up_shares
        } else {
            0.0
        }
    }
    pub fn down_avg(&self) -> f64 {
        if self.down_shares > 0.0 {
            self.down_spent / self.down_shares
        } else {
            0.0
        }
    }
    pub fn total_avg(&self) -> f64 {
        self.up_avg() + self.down_avg()
    }

    /// Возвращает абсолютный перекос портфеля (разницу между UP и DOWN акциями)
    pub fn skew(&self) -> f64 {
        (self.up_shares - self.down_shares).abs()
    }

    /// Возвращает направленный перекос относительно cheap стороны
    /// Положительный = больше cheap акций (нормально)
    /// Отрицательный = больше expensive акций (нужно увеличить лимит)
    pub fn directed_skew(&self, cheap_side: Side) -> f64 {
        match cheap_side {
            Side::Up => self.up_shares - self.down_shares,
            Side::Down => self.down_shares - self.up_shares,
        }
    }

    /// Возвращает информацию о cheap стороне для расчета ratio
    /// Если перекос положительный (больше cheap) - возвращает avg цену cheap стороны
    /// Если перекос отрицательный (больше expensive) - возвращает 0.0
    pub fn cheap_side_info(&self, cheap_side: Side) -> (Side, f64) {
        let directed_skew = self.directed_skew(cheap_side);
        if directed_skew > 0.0 {
            // Больше cheap акций - возвращаем avg цену
            match cheap_side {
                Side::Up => (Side::Up, self.up_avg()),
                Side::Down => (Side::Down, self.down_avg()),
            }
        } else {
            // Больше expensive акций или равно - avg = 0
            (cheap_side, 0.0)
        }
    }

    /// Возвращает сторону с меньшим количеством акций (expensive side)
    pub fn expensive_side(&self) -> Side {
        if self.up_shares >= self.down_shares {
            Side::Down
        } else {
            Side::Up
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct MarketPrices {
    pub up_bid: f64,
    pub up_bid_size: f64,
    pub up_bid_2: f64,      // Второй уровень UP bid
    pub up_bid_size_2: f64, // Размер второго уровня UP bid
    pub up_ask: f64,
    pub up_ask_size: f64,
    pub down_bid: f64,
    pub down_bid_size: f64,
    pub down_bid_2: f64,      // Второй уровень DOWN bid
    pub down_bid_size_2: f64, // Размер второго уровня DOWN bid
    pub down_ask: f64,
    pub down_ask_size: f64,
}

impl MarketPrices {
    /// Возвращает спред в центах (нормальный спред = 1)
    pub fn spread_cents(&self) -> i32 {
        let sum = self.up_bid + self.down_bid;
        // 0.99 = 1 цент спред, 0.98 = 2 цента спред, и т.д.
        ((1.0 - sum) * 100.0).round() as i32
    }

    /// Определяет сильную сторону (bb > 0.5)
    pub fn strong_side(&self) -> Side {
        if self.up_bid > self.down_bid {
            Side::Up
        } else {
            Side::Down
        }
    }

    /// Определяет слабую сторону (bb < 0.5)
    pub fn weak_side(&self) -> Side {
        self.strong_side().opposite()
    }

    /// Получить bb для указанной стороны
    pub fn bid_for_side(&self, side: Side) -> f64 {
        match side {
            Side::Up => self.up_bid,
            Side::Down => self.down_bid,
        }
    }

    /// Получить размер лучшего бида для указанной стороны
    pub fn bid_size_for_side(&self, side: Side) -> f64 {
        match side {
            Side::Up => self.up_bid_size,
            Side::Down => self.down_bid_size,
        }
    }
}

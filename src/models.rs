use serde::{Deserialize, Serialize};


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
pub enum Side { Up, Down }

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

    pub fn coinbase_product(&self) -> &'static str {
        match self {
            Coin::BTC => "BTC-USD",
            Coin::ETH => "ETH-USD",
            Coin::SOL => "SOL-USD",
            Coin::XRP => "XRP-USD",
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

#[derive(Debug, Default, Clone)]
pub struct Portfolio {
    pub up_shares: f64,
    pub down_shares: f64,
    pub up_spent: f64,
    pub down_spent: f64,
    pub maker_trades: u32,
    pub taker_trades: u32,
    // Отслеживание выставленных лимиток
    pub up_total_placed: f64,    // Всего shares выставлено в UP лимитках
    pub down_total_placed: f64,  // Всего shares выставлено в DOWN лимитках
}

#[allow(dead_code)]
impl Portfolio {
    pub fn up_avg(&self) -> f64 { if self.up_shares > 0.0 { self.up_spent / self.up_shares } else { 0.0 } }
    pub fn down_avg(&self) -> f64 { if self.down_shares > 0.0 { self.down_spent / self.down_shares } else { 0.0 } }
    pub fn total_avg(&self) -> f64 { self.up_avg() + self.down_avg() }

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
    pub up_bid_2: f64,          // Второй уровень UP bid
    pub up_bid_size_2: f64,     // Размер второго уровня UP bid
    pub up_ask: f64,
    pub up_ask_size: f64,
    pub down_bid: f64,
    pub down_bid_size: f64,
    pub down_bid_2: f64,        // Второй уровень DOWN bid
    pub down_bid_size_2: f64,   // Размер второго уровня DOWN bid
    pub down_ask: f64,
    pub down_ask_size: f64,
}
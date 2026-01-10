use serde::{Deserialize, Serialize};


#[derive(Debug, Deserialize, Clone)]
pub struct Market {
    #[serde(rename = "clobTokenIds")]
    pub clob_token_ids: String,
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

#[derive(Debug, Default, Clone)]
pub struct Portfolio {
    pub up_shares: f64,
    pub down_shares: f64,
    pub up_spent: f64,
    pub down_spent: f64,
    pub maker_trades: u32,
    pub taker_trades: u32,
}

#[allow(dead_code)]
impl Portfolio {
    pub fn up_avg(&self) -> f64 { if self.up_shares > 0.0 { self.up_spent / self.up_shares } else { 0.0 } }
    pub fn down_avg(&self) -> f64 { if self.down_shares > 0.0 { self.down_spent / self.down_shares } else { 0.0 } }
    pub fn total_avg(&self) -> f64 { self.up_avg() + self.down_avg() }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct MarketPrices {
    pub up_bid: f64,
    pub up_bid_size: f64,
    pub up_ask: f64,
    pub up_ask_size: f64,
    pub down_bid: f64,
    pub down_bid_size: f64,
    pub down_ask: f64,
    pub down_ask_size: f64,
}
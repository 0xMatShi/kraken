use std::sync::{Arc, Mutex};
use std::collections::{HashSet, HashMap};
use crate::models::{Portfolio, Side, MarketPrices};
use crate::utils::config::TradingConfig;
use crate::ui::{self, UiState};

use polymarket_client_sdk::clob::Client;
use polymarket_client_sdk::auth::Normal;
use polymarket_client_sdk::auth::state::Authenticated;
use alloy::signers::local::PrivateKeySigner;
use tracing::info;
use uuid::Uuid;

/// Трекер виртуального лимита для cheap side
#[derive(Debug, Clone)]
pub struct VirtualLimitTracker {
    pub used_shares: f64,
}

impl Default for VirtualLimitTracker {
    fn default() -> Self {
        Self { used_shares: 0.0 }
    }
}

/// Тип потока для отслеживания ордеров
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StreamType {
    CheapSide,
    ExpensiveSide,
}

/// Информация об активном ордере
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ActiveOrder {
    pub order_id: String,
    pub is_up: bool,
    pub price: f64,
    pub size: f64,
    pub filled: f64,
    pub stream_type: StreamType,
}

pub struct RealEngine {
    pub portfolio: Mutex<Portfolio>,
    pub virtual_limit: Mutex<VirtualLimitTracker>,
    pub active_orders: Mutex<HashMap<String, ActiveOrder>>,
    pub client: Client<Authenticated<Normal>>,
    pub signer: PrivateKeySigner,
    pub up_token: Arc<str>,
    pub down_token: Arc<str>,
    pub seen_trades: Mutex<HashSet<String>>,
    pub seen_orders: Mutex<HashSet<String>>,
    pub active_order_ids: Mutex<HashSet<String>>,
    pub active_orders_info: Mutex<HashMap<String, (f64, bool, f64, f64)>>,  // order_id -> (price, is_up, original_size, accumulated_filled)
    pub our_api_key: Uuid,
    pub config: TradingConfig,
    pub ui_state: UiState,
    pub last_prices: Mutex<Option<MarketPrices>>,
}

impl RealEngine {
    pub fn new(
        client: Client<Authenticated<Normal>>,
        signer: PrivateKeySigner,
        up_token: String,
        down_token: String,
        our_api_key: Uuid,
        config: TradingConfig,
        ui_state: UiState,
    ) -> Self {
        info!("🔧 Инициализация движка: Two-Stream Architecture");
        info!("   Cheap limit: {:.1} | Order size: {:.1} | Expiration: {}s",
            config.cheap_limit, config.size, config.expiration_seconds);

        Self {
            portfolio: Mutex::new(Portfolio::default()),
            virtual_limit: Mutex::new(VirtualLimitTracker::default()),
            active_orders: Mutex::new(HashMap::new()),
            client,
            signer,
            up_token: Arc::from(up_token.as_str()),
            down_token: Arc::from(down_token.as_str()),
            seen_trades: Mutex::new(HashSet::new()),
            seen_orders: Mutex::new(HashSet::new()),
            active_order_ids: Mutex::new(HashSet::new()),
            active_orders_info: Mutex::new(HashMap::new()),
            our_api_key,
            config,
            ui_state,
            last_prices: Mutex::new(None),
        }
    }

    // Обновить UI с текущим состоянием портфолио
    pub fn update_ui_portfolio(&self) {
        let port = self.portfolio.lock().unwrap();
        ui::update_portfolio(&self.ui_state, port.clone());
    }

    // Округление до 2 знаков (минимальный тик-размер 0.01)
    pub fn round_price(price: f64) -> f64 {
        (price * 100.0).round() / 100.0
    }

    // Методы для работы с активными ордерами
    pub fn add_order_id(&self, order_id: String) {
        let mut orders = self.active_order_ids.lock().unwrap();
        orders.insert(order_id);
    }

    pub fn remove_order_id(&self, order_id: &str) {
        let mut orders = self.active_order_ids.lock().unwrap();
        orders.remove(order_id);
    }

    /// Основной метод стратегии - точка входа для каждого тика рынка
    pub fn process_tick(self: &Arc<Self>, prices: MarketPrices) {
        // Сохраняем последние актуальные цены
        *self.last_prices.lock().unwrap() = Some(prices.clone());

        // Проверяем режим торговли
        let trading_enabled = {
            let state = self.ui_state.lock().unwrap();
            state.trading_enabled
        };

        if !trading_enabled {
            return;
        }

        // Определяем cheap и expensive sides по bid цене
        let (cheap_side, cheap_bid, expensive_side, expensive_bid) =
            self.detect_sides(&prices);

        // Edge case: если обе стороны cheap или обе expensive - ничего не делаем
        if cheap_side == expensive_side {
            return;
        }

        // Stream 1: Cheap Side (bid < 0.5)
        super::streams::run_cheap_side_stream(self, cheap_side, cheap_bid);

        // Stream 2: Expensive Side (bid > 0.5)
        super::streams::run_expensive_side_stream(
            self,
            cheap_side,
            cheap_bid,
            expensive_side,
            expensive_bid,
        );
    }

    /// Определяет cheap и expensive sides по bid ценам
    fn detect_sides(&self, prices: &MarketPrices) -> (Side, f64, Side, f64) {
        // cheap side = bid < 0.5
        // expensive side = bid > 0.5
        if prices.up_bid < 0.5 && prices.down_bid > 0.5 {
            (Side::Up, prices.up_bid, Side::Down, prices.down_bid)
        } else if prices.down_bid < 0.5 && prices.up_bid > 0.5 {
            (Side::Down, prices.down_bid, Side::Up, prices.up_bid)
        } else if prices.up_bid < 0.5 && prices.down_bid < 0.5 {
            // Обе стороны cheap - выбираем более дешёвую
            if prices.up_bid <= prices.down_bid {
                (Side::Up, prices.up_bid, Side::Up, prices.up_bid)
            } else {
                (Side::Down, prices.down_bid, Side::Down, prices.down_bid)
            }
        } else {
            // Обе стороны expensive - выбираем менее дорогую как expensive
            if prices.up_bid <= prices.down_bid {
                (Side::Down, prices.down_bid, Side::Up, prices.up_bid)
            } else {
                (Side::Up, prices.up_bid, Side::Down, prices.down_bid)
            }
        }
    }

    /// Финализация сессии - генерация отчета
    pub fn finalize(&self, final_prices: &MarketPrices) {
        let port = self.portfolio.lock().unwrap();
        let winner = if final_prices.up_bid > 0.5 { Side::Up } else { Side::Down };
        let winning_shares = if winner == Side::Up { port.up_shares } else { port.down_shares };
        let total_spent = port.up_spent + port.down_spent;
        let pnl = winning_shares - total_spent;

        info!("=== FINAL REPORT ===");
        info!("Winner: {:?}", winner);
        info!("Shares Held: {:.2}", winning_shares);
        info!("Cost Basis: ${:.2}", total_spent);
        info!("PnL: ${:.2}", pnl);
        info!("Maker Trades: {}", port.maker_trades);
        info!("Taker Trades: {}", port.taker_trades);
    }
}

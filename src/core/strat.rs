use std::sync::{Arc, Mutex};
use std::collections::{HashSet, HashMap};
use crate::models::{Portfolio, Side, MarketPrices};
use crate::utils::config::TradingConfig;
use crate::ui::{self, UiState};

use polymarket_client_sdk::clob::Client;
use polymarket_client_sdk::auth::Normal;
use polymarket_client_sdk::auth::state::Authenticated;
use alloy::signers::local::PrivateKeySigner;
use tracing::{info, warn};
use uuid::Uuid;

pub struct RealEngine {
    pub portfolio: Mutex<Portfolio>,
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
    pub profit_target_reached: Mutex<bool>,
    /// Счетчик активных лимиток (увеличивается при размещении, уменьшается при полном FILL или CANCELLATION)
    pub active_orders_count: Mutex<usize>,
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
        info!("🔧 Инициализация движка: Новая логика размещения ордеров");
        info!("   Order size: {:.1} | Max active orders: {}",
            config.size, config.max_active_orders);

        Self {
            portfolio: Mutex::new(Portfolio::default()),
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
            profit_target_reached: Mutex::new(false),
            active_orders_count: Mutex::new(0),
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

    /// Увеличивает счетчик активных лимиток при размещении
    pub fn increment_orders_count(&self) {
        let mut count = self.active_orders_count.lock().unwrap();
        *count += 1;
        info!("📊 Активных лимиток: {}/{}", *count, self.config.max_active_orders);
    }

    /// Уменьшает счетчик активных лимиток при полном FILL или CANCELLATION
    pub fn decrement_orders_count(&self) {
        let mut count = self.active_orders_count.lock().unwrap();
        if *count > 0 {
            *count -= 1;
        }
        info!("📊 Активных лимиток: {}/{}", *count, self.config.max_active_orders);
    }

    /// Проверяет, можно ли разместить новую лимитку
    pub fn can_place_order(&self) -> bool {
        let count = self.active_orders_count.lock().unwrap();
        *count < self.config.max_active_orders
    }

    /// Проверяет условие прибыльности: прибыль с каждой стороны > $3
    pub fn check_profit_target(&self) -> bool {
        let port = self.portfolio.lock().unwrap();

        // Прибыль = shares - spent
        let total_spent = port.up_spent + port.down_spent;
        let up_profit = port.up_shares - total_spent;
        let down_profit = port.down_shares - total_spent;

        info!("💰 Profit Check: UP profit: ${:.2} | DOWN profit: ${:.2}", up_profit, down_profit);

        // Если обе стороны имеют прибыль > $3
        if up_profit > 5.0 && down_profit > 5.0 {
            info!("🎉 PROFIT TARGET REACHED! UP: ${:.2} | DOWN: ${:.2}", up_profit, down_profit);
            return true;
        }

        false
    }

    /// Отменяет все активные ордера
    pub async fn cancel_all_orders(&self) {
        info!("🛑 Отменяем все активные ордера...");

        match self.client.cancel_all_orders().await {
            Ok(_) => {
                info!("✅ Все ордера успешно отменены");

                // Очищаем внутренние структуры
                self.active_order_ids.lock().unwrap().clear();
                self.active_orders_info.lock().unwrap().clear();
            }
            Err(e) => {
                warn!("❌ Ошибка отмены ордеров: {}", e);
            }
        }
    }

    /// Основной метод стратегии - точка входа для каждого тика рынка
    pub fn process_tick(self: &Arc<Self>, prices: MarketPrices) {
        // Сохраняем последние актуальные цены
        *self.last_prices.lock().unwrap() = Some(prices.clone());

        // Проверяем режим торговли
        let trading_mode = {
            let state = self.ui_state.lock().unwrap();
            state.trading_mode
        };

        match trading_mode {
            ui::TradingMode::Stop => {
                // Stop режим: ничего не делаем, софт стоит афк
                return;
            }
            ui::TradingMode::RealRun => {
                // RealRun режим: реальная торговля
            }
        }

        // Проверяем флаг достижения прибыли
        {
            let target_reached = self.profit_target_reached.lock().unwrap();
            if *target_reached {
                // Прибыль уже достигнута, ордера отменены, ничего не делаем
                return;
            }
        }

        // Проверяем условие прибыльности
        if self.check_profit_target() {
            // Устанавливаем флаг
            *self.profit_target_reached.lock().unwrap() = true;

            // Отменяем все ордера асинхронно
            let engine_clone = Arc::clone(self);
            tokio::spawn(async move {
                engine_clone.cancel_all_orders().await;

                // Переводим бота в режим STOP
                {
                    let mut state = engine_clone.ui_state.lock().unwrap();
                    state.trading_mode = ui::TradingMode::Stop;
                }

                info!("🛑 Бот остановлен. Profit target достигнут.");
            });

            return;
        }

        // Размещаем ордера согласно новой логике (без таймера, на каждый тик)
        super::streams::process_order_placement(self, prices);
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

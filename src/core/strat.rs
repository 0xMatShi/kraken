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
    pub active_orders_info: Mutex<HashMap<String, (f64, bool, f64, f64)>>,  // order_id -> (price, is_up, original_size, accumulated_filled)
    /// Отслеживание неисполненных ордеров по сторонам для замены
    /// Хранит (order_id, price) для проверки нужности замены
    pub pending_up_orders: Mutex<Vec<(String, f64)>>,    // (order_id, price) на UP стороне
    pub pending_down_orders: Mutex<Vec<(String, f64)>>,  // (order_id, price) на DOWN стороне
    pub our_api_key: Uuid,
    pub config: TradingConfig,
    pub ui_state: UiState,
    pub last_prices: Mutex<Option<MarketPrices>>,
    pub profit_target_reached: Mutex<bool>,
    /// Последние обработанные цены тика для дедупликации (up_bid, down_bid)
    pub last_tick_prices: Mutex<Option<(f64, f64)>>,
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
            active_orders_info: Mutex::new(HashMap::new()),
            our_api_key,
            config,
            ui_state,
            last_prices: Mutex::new(None),
            profit_target_reached: Mutex::new(false),
            pending_up_orders: Mutex::new(Vec::new()),
            pending_down_orders: Mutex::new(Vec::new()),
            last_tick_prices: Mutex::new(None),
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


    /// Добавляет ордер в список неисполненных по стороне с ценой
    pub fn add_pending_order(&self, order_id: String, is_up: bool, price: f64) {
        if is_up {
            let mut orders = self.pending_up_orders.lock().unwrap();
            orders.push((order_id, price));
        } else {
            let mut orders = self.pending_down_orders.lock().unwrap();
            orders.push((order_id, price));
        }
    }

    /// Удаляет ордер из списка неисполненных при FILL или CANCELLATION
    pub fn remove_pending_order(&self, order_id: &str, is_up: bool) {
        if is_up {
            let mut orders = self.pending_up_orders.lock().unwrap();
            orders.retain(|(id, _)| id != order_id);
        } else {
            let mut orders = self.pending_down_orders.lock().unwrap();
            orders.retain(|(id, _)| id != order_id);
        }
    }

    /// Получает список неисполненных ордеров (UP, DOWN) с ценами
    pub fn get_pending_orders(&self) -> (Vec<(String, f64)>, Vec<(String, f64)>) {
        let up = self.pending_up_orders.lock().unwrap().clone();
        let down = self.pending_down_orders.lock().unwrap().clone();
        (up, down)
    }

    /// Очищает списки неисполненных ордеров
    pub fn clear_pending_orders(&self) {
        self.pending_up_orders.lock().unwrap().clear();
        self.pending_down_orders.lock().unwrap().clear();
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

        // Проверяем дедупликацию по ценам тика
        let current_tick = (prices.up_bid, prices.down_bid);
        {
            let mut last_tick = self.last_tick_prices.lock().unwrap();

            // Если цены не изменились - скипаем обработку
            if let Some(prev_tick) = *last_tick {
                if prev_tick == current_tick {
                    info!("⏭️ Тик с теми же ценами: UP {:.3} | DOWN {:.3} - скипаем", current_tick.0, current_tick.1);
                    return;
                }
            }

            // Сохраняем новые цены
            *last_tick = Some(current_tick);
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

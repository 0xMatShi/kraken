use std::sync::{Arc, Mutex};
use std::collections::{HashSet, HashMap};
use std::fs;
use std::path::Path;
use crate::models::{Portfolio, Side, MarketPrices};
use crate::utils::config::TradingConfig;
use crate::ui::{self, UiState};
use serde::{Serialize, Deserialize};

use polymarket_client_sdk::clob::Client;
use polymarket_client_sdk::auth::Normal;
use polymarket_client_sdk::auth::state::Authenticated;
use alloy::signers::local::PrivateKeySigner;
use tracing::{info, warn, error};
use uuid::Uuid;

#[derive(Serialize, Deserialize)]
pub struct ClaimData {
    pub condition_id: String,
    pub winning_outcome_index: u8,  // 0 = UP/YES, 1 = DOWN/NO
}

/// Состояние хеджирования
#[derive(Debug, Clone)]
pub struct HedgeState {
    pub order_id: Option<String>,        // ID текущей лимитки хеджа
    pub is_up_side: bool,                // Какую сторону хеджируем (true = покупаем UP)
    pub current_price: f64,              // По какой цене размещен текущий ордер
    pub target_size: f64,                // Целевое количество акций для хеджа
    pub filled_size: f64,                // Сколько уже исполнено
    pub pending_repricing: Option<(f64, f64)>,  // Ожидающее перевыставление: (new_price, new_size) после WebSocket CANCELLATION
}

/// Состояние торговой стратегии для одного потока
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum TradingState {
    /// Ждём спред 2с (up_bid + down_bid == 0.98)
    Idle,
    /// Первая нога размещена, ждём заполнения
    WaitingFirstLeg {
        order_id: String,
        is_up: bool,
        price: f64,
        size: f64,
    },
    /// Первая нога заполнена, ищем вторую ногу
    /// Мониторим ask для хеджа + размещена лимитка второй ноги
    SearchingSecondLeg {
        first_leg_price: f64,
        first_leg_is_up: bool,
        first_leg_size: f64,
        second_leg_order_id: Option<String>,
        second_leg_current_price: Option<f64>,   // Цена по которой размещен текущий ордер второй ноги
        second_leg_filled: f64,                  // Сколько уже исполнено из второй ноги
        pending_repricing: Option<f64>,          // Ожидающее перевыставление: new_price после WebSocket CANCELLATION
    },
}

impl Default for TradingState {
    fn default() -> Self {
        TradingState::Idle
    }
}

/// Ключ для бронирования цены (is_up, price_cents)
/// price_cents = (price * 100).round() as i32
pub type ReservedPriceKey = (bool, i32);

pub fn price_to_cents(price: f64) -> i32 {
    (price * 100.0).round() as i32
}

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
    pub condition_id: Option<String>,
    // Многопоточность: N потоков на каждую сторону (N = config.threads)
    pub up_threads: Vec<Arc<Mutex<TradingState>>>,
    pub down_threads: Vec<Arc<Mutex<TradingState>>>,
    pub reserved_prices: Mutex<HashSet<ReservedPriceKey>>,
    pub last_prices: Mutex<Option<MarketPrices>>,
    pub hedge_state: Mutex<Option<HedgeState>>,
    pub hedging_active: Mutex<bool>,
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
        condition_id: Option<String>,
    ) -> Self {
        // Создаём N потоков на каждую сторону (N = config.threads)
        let num_threads = config.threads.max(1);

        let up_threads: Vec<Arc<Mutex<TradingState>>> = (0..num_threads)
            .map(|_| Arc::new(Mutex::new(TradingState::Idle)))
            .collect();

        let down_threads: Vec<Arc<Mutex<TradingState>>> = (0..num_threads)
            .map(|_| Arc::new(Mutex::new(TradingState::Idle)))
            .collect();

        info!("🔧 Инициализация движка: {} UP-потоков + {} DOWN-потоков = {} всего",
            num_threads, num_threads, num_threads * 2);

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
            condition_id,
            up_threads,
            down_threads,
            reserved_prices: Mutex::new(HashSet::new()),
            last_prices: Mutex::new(None),
            hedge_state: Mutex::new(None),
            hedging_active: Mutex::new(false),
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

    /// Проверяем, есть ли активные потоки (НЕ в состоянии Idle)
    pub fn has_active_threads(&self) -> bool {
        let mut active_count = 0;

        for (idx, thread) in self.up_threads.iter().enumerate() {
            let state = thread.lock().unwrap();
            if !matches!(*state, TradingState::Idle) {
                active_count += 1;
                info!("🔍 UP-поток #{} НЕ в Idle", idx + 1);
            }
        }

        for (idx, thread) in self.down_threads.iter().enumerate() {
            let state = thread.lock().unwrap();
            if !matches!(*state, TradingState::Idle) {
                active_count += 1;
                info!("🔍 DOWN-поток #{} НЕ в Idle", idx + 1);
            }
        }

        if active_count > 0 {
            info!("🔍 Всего активных потоков: {}", active_count);
        }

        active_count > 0
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
        // Сохраняем последние актуальные цены для размещения второй ноги
        *self.last_prices.lock().unwrap() = Some(prices.clone());

        // Проверяем режим торговли
        let trading_enabled = {
            let state = self.ui_state.lock().unwrap();
            state.trading_enabled
        };

        if !trading_enabled {
            return;
        }

        // ПРИОРИТЕТ 1: Проверяем необходимость хеджирования ПЕРЕД основным алгоритмом
        super::hedge::check_and_start_hedge(self, &prices);

        // ПРИОРИТЕТ 2: Основной алгоритм - только если хедж не активен
        let hedging_active = *self.hedging_active.lock().unwrap();
        if !hedging_active {
            self.run_logic(prices.clone());
        }

        // ПРИОРИТЕТ 2.5: Перевыставление СУЩЕСТВУЮЩИХ вторых ног - работает ВСЕГДА
        super::second_leg::check_second_leg_repricing(self, &prices);

        // ПРИОРИТЕТ 3: Проверяем перевыставление хеджа (если он активен)
        super::hedge::check_hedge_repricing(self, &prices);
    }

    /// Основная логика стратегии - размещение первых ног для свободных потоков
    fn run_logic(&self, prices: MarketPrices) {
        let (up_spent, down_spent) = {
            let port = self.portfolio.lock().unwrap();
            (port.up_spent, port.down_spent)
        };

        if (up_spent + down_spent) >= self.config.max_balance {
            return;
        }

        // === UP-потоки: первая нога всегда UP ===
        for (thread_idx, trading_state) in self.up_threads.iter().enumerate() {
            let state = trading_state.lock().unwrap().clone();

            if let TradingState::Idle = state {
                super::first_leg::try_place_first_leg_for_thread(
                    self,
                    thread_idx,
                    &prices,
                    true,
                    trading_state
                );
            }
        }

        // === DOWN-потоки: первая нога всегда DOWN ===
        for (thread_idx, trading_state) in self.down_threads.iter().enumerate() {
            let state = trading_state.lock().unwrap().clone();

            if let TradingState::Idle = state {
                super::first_leg::try_place_first_leg_for_thread(
                    self,
                    thread_idx,
                    &prices,
                    false,
                    trading_state
                );
            }
        }
    }

    /// Финализация сессии - генерация отчета и сохранение данных для клейма
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

        // Сохраняем данные для клейма наград
        if let Some(ref cond_id) = self.condition_id {
            let winning_outcome_index = if winner == Side::Up { 0 } else { 1 };
            let claim_data = ClaimData {
                condition_id: cond_id.clone(),
                winning_outcome_index,
            };

            let redeem_dir = Path::new("src/redeem");
            if let Err(e) = fs::create_dir_all(redeem_dir) {
                error!("❌ Ошибка создания директории src/redeem: {}", e);
                return;
            }

            let claim_path = redeem_dir.join("claim.json");
            let mut claim_events: Vec<ClaimData> = if claim_path.exists() {
                match fs::read_to_string(&claim_path) {
                    Ok(content) => {
                        serde_json::from_str(&content).unwrap_or_else(|e| {
                            warn!("⚠️ Ошибка парсинга claim.json: {}. Создаем новый массив", e);
                            Vec::new()
                        })
                    },
                    Err(e) => {
                        warn!("⚠️ Ошибка чтения claim.json: {}. Создаем новый массив", e);
                        Vec::new()
                    }
                }
            } else {
                Vec::new()
            };

            claim_events.push(claim_data);

            match serde_json::to_string_pretty(&claim_events) {
                Ok(json_str) => {
                    if let Err(e) = fs::write(&claim_path, json_str) {
                        error!("❌ Ошибка записи claim.json: {}", e);
                    } else {
                        info!("💾 Событие добавлено в claim.json ({:?})", claim_path);
                        info!("   Condition ID: {}", cond_id);
                        info!("   Winning Outcome: {} ({})", winning_outcome_index, if winning_outcome_index == 0 { "UP/YES" } else { "DOWN/NO" });
                        info!("   Всего событий в очереди: {}", claim_events.len());
                    }
                },
                Err(e) => error!("❌ Ошибка сериализации claim data: {}", e),
            }
        } else {
            warn!("⚠️ Condition ID не найден, пропускаем сохранение claim.json");
        }
    }
}

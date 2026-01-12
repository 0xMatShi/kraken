use std::sync::{Arc, Mutex};
use std::collections::{HashSet, HashMap};
use std::fs;
use std::path::Path;
use crate::models::{Portfolio, Side, MarketPrices};
use crate::config::TradingConfig;
use crate::ui::{self, UiState, TradeHistoryEntry, TradeType, OpenOrder};
use serde::{Serialize, Deserialize};

use polymarket_client_sdk::clob::Client;
use polymarket_client_sdk::auth::Normal;
use polymarket_client_sdk::auth::state::Authenticated;
use polymarket_client_sdk::clob::types::{OrderType, Side as PolySide};
use polymarket_client_sdk::types::Decimal;
use alloy::signers::local::PrivateKeySigner;
use tracing::{info, warn, error};
use uuid::Uuid;
use chrono::Utc;

#[derive(Serialize, Deserialize)]
struct ClaimData {
    condition_id: String,
    winning_outcome_index: u8,  // 0 = UP/YES, 1 = DOWN/NO
}

/// Состояние торговой стратегии
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
    },
}

impl Default for TradingState {
    fn default() -> Self {
        TradingState::Idle
    }
}

pub struct RealEngine {
    portfolio: Mutex<Portfolio>,
    client: Client<Authenticated<Normal>>,
    signer: PrivateKeySigner,
    up_token: Arc<str>,
    down_token: Arc<str>,
    seen_trades: Mutex<HashSet<String>>,
    seen_orders: Mutex<HashSet<String>>,
    active_order_ids: Mutex<HashSet<String>>,
    active_orders_info: Mutex<HashMap<String, (f64, bool, f64, f64)>>,  // order_id -> (price, is_up, original_size, accumulated_filled)
    our_api_key: Uuid,
    config: TradingConfig,
    // UI state для отображения портфолио и контроля режима торговли
    ui_state: UiState,
    // Condition ID для клейма наград
    condition_id: Option<String>,
    // Состояние торговой стратегии (Arc для передачи в async блоки)
    trading_state: Arc<Mutex<TradingState>>,
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
            trading_state: Arc::new(Mutex::new(TradingState::Idle)),
        }
    }

    // Обновить UI с текущим состоянием портфолио
    fn update_ui_portfolio(&self) {
        let port = self.portfolio.lock().unwrap();
        ui::update_portfolio(&self.ui_state, port.clone());
    }

    // Округление до 2 знаков (минимальный тик-размер 0.01)
    fn round_price(price: f64) -> f64 {
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

    pub fn process_tick(&self, prices: MarketPrices) {
        // Проверяем режим торговли - если торговля выключена, не размещаем ордера
        let trading_enabled = {
            let state = self.ui_state.lock().unwrap();
            state.trading_enabled
        };

        if !trading_enabled {
            return;
        }

        self.run_logic(prices);
    }

    fn run_logic(&self, prices: MarketPrices) {
        let (up_spent, down_spent) = {
            let port = self.portfolio.lock().unwrap();
            (port.up_spent, port.down_spent)
        };

        if (up_spent + down_spent) >= self.config.max_balance {
            return;
        }

        // Получаем текущее состояние стратегии
        let state = {
            self.trading_state.lock().unwrap().clone()
        };

        match state {
            TradingState::Idle => {
                // Ждём спред 2с (up_bid + down_bid == 0.98)
                self.try_place_first_leg(&prices);
            }
            TradingState::WaitingFirstLeg { .. } => {
                // Первая нога размещена, ждём заполнения через WebSocket
                // Ничего не делаем в tick
            }
            TradingState::SearchingSecondLeg { first_leg_price, first_leg_is_up, first_leg_size, second_leg_order_id } => {
                // Мониторим ask противоположной стороны для хеджа
                self.check_hedge_opportunity(&prices, first_leg_price, first_leg_is_up, first_leg_size, second_leg_order_id);
            }
        }
    }

    /// Пытаемся разместить первую ногу при спреде 2с
    fn try_place_first_leg(&self, prices: &MarketPrices) {
        // Проверяем валидность цен
        if prices.up_bid < 0.01 || prices.down_bid < 0.01 {
            return;
        }

        // Проверяем спред: up_bid + down_bid должно быть ровно 0.98
        let pair_cost = prices.up_bid + prices.down_bid;
        if (pair_cost - 0.98).abs() > 0.001 {
            return; // Спред не 2с
        }

        // Определяем сторону для первой ноги: bid > 0.50
        let (first_leg_is_up, first_leg_price) = if prices.up_bid > 0.50 {
            (true, Self::round_price(prices.up_bid + 0.01))
        } else if prices.down_bid > 0.50 {
            (false, Self::round_price(prices.down_bid + 0.01))
        } else {
            // Обе стороны <= 0.50, не размещаем
            return;
        };

        if first_leg_price < 0.01 || first_leg_price > 0.99 {
            return;
        }

        info!("🎯 Спред 2с найден! Размещаем первую ногу: {} @ {:.2}",
            if first_leg_is_up { "UP" } else { "DOWN" }, first_leg_price);

        // Переходим в состояние ожидания сразу, чтобы не дублировать ордера
        // order_id будет обновлён при получении PLACEMENT через WebSocket
        *self.trading_state.lock().unwrap() = TradingState::WaitingFirstLeg {
            order_id: String::new(), // Заполнится при PLACEMENT
            is_up: first_leg_is_up,
            price: first_leg_price,
            size: self.config.size,
        };

        let token_id = if first_leg_is_up {
            Arc::clone(&self.up_token)
        } else {
            Arc::clone(&self.down_token)
        };
        let client = self.client.clone();
        let signer = self.signer.clone();
        let size = self.config.size;

        // Клонируем Arc на trading_state для использования в async блоке
        let trading_state = Arc::clone(&self.trading_state);

        tokio::spawn(async move {
            let price_dec: Decimal = format!("{:.2}", first_leg_price).parse().unwrap();
            let size_dec: Decimal = format!("{:.2}", size).parse().unwrap();

            // Экспирация: now + 60 + 10 секунд (чтобы ордер не висел вечно если цена убежит)
            let expiration = Utc::now() + chrono::Duration::seconds(60 + 10);

            let order = client.limit_order()
                .token_id(token_id.as_ref())
                .price(price_dec)
                .size(size_dec)
                .side(PolySide::Buy)
                .order_type(OrderType::GTD)
                .expiration(expiration)
                .build().await.unwrap();

            let signed = client.sign(&signer, order).await.unwrap();

            match client.post_order(signed).await {
                Ok(response) => {
                    if !response.order_id.is_empty() {
                        info!("📝 Первая нога размещена: order_id={}", response.order_id);

                        // Обновляем order_id в trading_state
                        let mut state = trading_state.lock().unwrap();
                        if let TradingState::WaitingFirstLeg { ref mut order_id, .. } = *state {
                            *order_id = response.order_id;
                        }
                    }
                },
                Err(e) => error!("❌ Ошибка размещения первой ноги: {}", e),
            }
        });
    }

    /// Проверяем возможность хеджа и управляем второй ногой
    fn check_hedge_opportunity(
        &self,
        prices: &MarketPrices,
        first_leg_price: f64,
        first_leg_is_up: bool,
        first_leg_size: f64,
        second_leg_order_id: Option<String>,
    ) {
        // Получаем ask противоположной стороны
        let opposite_ask = if first_leg_is_up {
            prices.down_ask
        } else {
            prices.up_ask
        };

        // Проверяем условие хеджа: first_leg_price + opposite_ask >= 1.02
        if opposite_ask > 0.0 && (first_leg_price + opposite_ask) >= 1.02 {
            info!("🚨 ХЕДЖ УСЛОВИЕ! Первая нога: {:.2} + Ask: {:.2} = {:.2} >= 1.02",
                first_leg_price, opposite_ask, first_leg_price + opposite_ask);

            // Отправляем taker хедж
            self.execute_hedge_taker(first_leg_is_up, first_leg_size);

            // Отменяем лимитку второй ноги если она есть
            if let Some(order_id) = second_leg_order_id {
                self.cancel_order(order_id);
            }

            // Возвращаемся в Idle
            *self.trading_state.lock().unwrap() = TradingState::Idle;
            return;
        }
    }

    /// Выполняем taker хедж сделку
    fn execute_hedge_taker(&self, first_leg_is_up: bool, size: f64) {
        // Хедж на противоположную сторону
        let hedge_is_up = !first_leg_is_up;
        let token_id = if hedge_is_up {
            Arc::clone(&self.up_token)
        } else {
            Arc::clone(&self.down_token)
        };
        let client = self.client.clone();
        let signer = self.signer.clone();

        info!("🎯 Отправляем TAKER HEDGE: {} size={:.2}",
            if hedge_is_up { "UP" } else { "DOWN" }, size);

        tokio::spawn(async move {
            let price_dec: Decimal = "0.99".parse().unwrap();
            let size_dec: Decimal = format!("{:.2}", size).parse().unwrap();

            let order = client.limit_order()
                .token_id(token_id.as_ref())
                .price(price_dec)
                .size(size_dec)
                .side(PolySide::Buy)
                .order_type(OrderType::GTC) // Fill or Kill для taker
                .build().await.unwrap();

            let signed = client.sign(&signer, order).await.unwrap();

            match client.post_order(signed).await {
                Ok(response) => {
                    if !response.order_id.is_empty() {
                        info!("✅ HEDGE TAKER отправлен: order_id={}", response.order_id);
                    }
                },
                Err(e) => error!("❌ Ошибка отправки HEDGE TAKER: {}", e),
            }
        });
    }

    /// Размещаем лимитку второй ноги
    fn place_second_leg(&self, first_leg_price: f64, first_leg_is_up: bool, first_leg_size: f64) {
        // Цена второй ноги: 0.98 - first_leg_price
        let second_leg_price = Self::round_price(0.99 - first_leg_price);
        let second_leg_is_up = !first_leg_is_up;

        if second_leg_price < 0.01 || second_leg_price > 0.99 {
            warn!("⚠️ Некорректная цена второй ноги: {:.2}", second_leg_price);
            return;
        }

        info!("📝 Размещаем вторую ногу: {} @ {:.2}",
            if second_leg_is_up { "UP" } else { "DOWN" }, second_leg_price);

        let token_id = if second_leg_is_up {
            Arc::clone(&self.up_token)
        } else {
            Arc::clone(&self.down_token)
        };
        let client = self.client.clone();
        let signer = self.signer.clone();
        let size = first_leg_size;

        tokio::spawn(async move {
            let price_dec: Decimal = format!("{:.2}", second_leg_price).parse().unwrap();
            let size_dec: Decimal = format!("{:.2}", size).parse().unwrap();

            let order = client.limit_order()
                .token_id(token_id.as_ref())
                .price(price_dec)
                .size(size_dec)
                .side(PolySide::Buy)
                .order_type(OrderType::GTC)
                .build().await.unwrap();

            let signed = client.sign(&signer, order).await.unwrap();

            match client.post_order(signed).await {
                Ok(response) => {
                    if !response.order_id.is_empty() {
                        info!("📝 Вторая нога размещена: order_id={}", response.order_id);
                    }
                },
                Err(e) => error!("❌ Ошибка размещения второй ноги: {}", e),
            }
        });
    }

    /// Отменяем ордер по ID
    fn cancel_order(&self, order_id: String) {
        let client = self.client.clone();

        info!("🚫 Отменяем ордер: {}", order_id);

        tokio::spawn(async move {
            match client.cancel_order(&order_id).await {
                Ok(result) => {
                    if !result.canceled.is_empty() {
                        info!("✅ Ордер отменён: {}", order_id);
                    } else {
                        warn!("⚠️ Ордер не был отменён: {}", order_id);
                    }
                },
                Err(e) => error!("❌ Ошибка отмены ордера {}: {}", order_id, e),
            }
        });
    }

    // Trade события = TAKER сделки (market orders FAK)
    // Это подтверждение исполнения taker-hedge и taker-emergency ордеров
    // ВАЛИДАЦИЯ: Проверяем по trade_owner
    pub fn handle_ws_trade(
        &self,
        trade_id: String,
        price: f64,
        size: f64,
        side: PolySide,
        asset_id: &str,
        trade_owner: Option<Uuid>,
        taker_order_id: Option<String>,
    ) {
        // ПРОВЕРКА 1: trade_owner должен совпадать с нашим API key
        let owner_matches = trade_owner.map_or(false, |owner| owner == self.our_api_key);

        if !owner_matches { return; }

        // Проверяем дубликаты (дополнительная защита)
        {
            let mut seen = self.seen_trades.lock().unwrap();
            if seen.contains(&trade_id) {
                return;
            }
            seen.insert(trade_id.clone());
        }

        let mut port = self.portfolio.lock().unwrap();

        port.taker_trades += 1;

        let is_buy = matches!(side, PolySide::Buy);

        if asset_id == &*self.up_token {
            if is_buy {
                port.up_shares += size;
                port.up_spent += price * size;
                port.up_total_placed += size;  // Тейкер тоже считается как "выставленный"
            } else {
                port.up_shares -= size;
                port.up_spent -= price * size;
            }
        } else if asset_id == &*self.down_token {
            if is_buy {
                port.down_shares += size;
                port.down_spent += price * size;
                port.down_total_placed += size;  // Тейкер тоже считается как "выставленный"
            } else {
                port.down_shares -= size;
                port.down_spent -= price * size;
            }
        }

        let side_str = if is_buy { "BUY" } else { "SELL" };
        let token_str = if asset_id == &*self.up_token { "UP" } else { "DOWN" };
        let is_up = asset_id == &*self.up_token;

        info!("✅ TAKER FILLED: {} {} @ {:.3} | Size: {:.2} | Cost: ${:.2}",
            side_str, token_str, price, size, price * size);
        info!("💰 Portfolio: UP {:.1} | DOWN {:.1} | Skew {:.1}",
            port.up_shares, port.down_shares, port.up_shares - port.down_shares);

        drop(port);

        // Добавляем запись в историю торговли
        if is_buy {
            let history_entry = TradeHistoryEntry {
                is_up,
                shares: size,
                price,
                cost: price * size,
                trade_type: TradeType::Taker,
                timestamp: Utc::now(),
            };
            ui::add_trade_history(&self.ui_state, history_entry);
        }

        // Обновляем UI
        self.update_ui_portfolio();

        // === ЛОГИКА СОСТОЯНИЯ СТРАТЕГИИ ДЛЯ TAKER FILLS ===
        // Если лимитка пересекла спред и исполнилась как тейкер,
        // мы должны распознать это как часть стратегии
        if is_buy {
            if let Some(ref order_id) = taker_order_id {
                self.handle_taker_fill_for_strategy(order_id, is_up, price, size);
            }
        }
    }

    /// Обработка taker fill в контексте стратегии
    /// Если мы в WaitingFirstLeg и taker fill совпадает с первой ногой,
    /// переходим в SearchingSecondLeg
    fn handle_taker_fill_for_strategy(&self, taker_order_id: &str, is_up: bool, price: f64, size: f64) {
        let mut state = self.trading_state.lock().unwrap();

        match &*state {
            TradingState::WaitingFirstLeg { order_id: first_leg_order_id, is_up: expected_is_up, price: expected_price, size: expected_size } => {
                // ВАЖНО: WebSocket событие может прийти раньше, чем REST API вернет order_id
                // Если order_id ещё пуст - сравниваем по атрибутам (is_up, price, size)
                let is_our_first_leg = if first_leg_order_id.is_empty() {
                    // order_id ещё не получен от REST API - сравниваем по атрибутам
                    let matches = is_up == *expected_is_up &&
                        (price - expected_price).abs() < 0.02 &&  // Погрешность для цены
                        (size - expected_size).abs() < 0.01;
                    if matches {
                        info!("🔄 TAKER FILL распознан по атрибутам (order_id ещё не получен)");
                    }
                    matches
                } else {
                    // order_id известен - точное сравнение
                    taker_order_id == first_leg_order_id
                };

                if is_our_first_leg {
                    info!("🔄 TAKER FILL = ПЕРВАЯ НОГА! order_id={}", taker_order_id);
                    info!("   {} @ {:.2} size={:.2} → SearchingSecondLeg",
                        if is_up { "UP" } else { "DOWN" }, price, size);

                    let first_leg_price = price;
                    let first_leg_is_up = is_up;
                    let first_leg_size = size;

                    *state = TradingState::SearchingSecondLeg {
                        first_leg_price,
                        first_leg_is_up,
                        first_leg_size,
                        second_leg_order_id: None,
                    };
                    drop(state);

                    // Размещаем вторую ногу
                    self.place_second_leg(first_leg_price, first_leg_is_up, first_leg_size);
                }
            }
            _ => {
                // В других состояниях taker fill не влияет на стратегию
            }
        }
    }

    // Обработка событий ордеров (MAKER orders - limit orders)
    // PLACEMENT - ордер размещён
    // UPDATE - ордер частично/полностью исполнен (some of it is matched)
    // CANCELLATION - ордер отменён
    pub fn handle_ws_order(&self, order_id: String, msg_type: Option<String>, price: f64, side: PolySide, asset_id: &str, size_matched: Option<f64>, original_size: Option<f64>) {
        // Дедупликация только для PLACEMENT и CANCELLATION
        // UPDATE события НЕ дедуплицируются, так как ордер может исполняться частями
        if msg_type.as_deref() != Some("UPDATE") {
            let order_key = format!("{}:{:?}", order_id, msg_type);
            let mut seen = self.seen_orders.lock().unwrap();
            if seen.contains(&order_key) {
                return; // Молча игнорируем дубликаты PLACEMENT/CANCELLATION
            }
            seen.insert(order_key);
        }

        let side_str = match side {
            PolySide::Buy => "BUY",
            PolySide::Sell => "SELL",
            _ => "UNKNOWN",
        };

        let token_str = if asset_id == &*self.up_token { "UP" } else { "DOWN" };

        match msg_type.as_deref() {
            Some("PLACEMENT") => {
                // Сохраняем ID нашего ордера
                self.add_order_id(order_id.clone());

                // Определяем is_up (UP или DOWN токен)
                let is_up = asset_id == &*self.up_token;

                // Сохраняем информацию об ордере (цена, сторона, original_size и accumulated_filled = 0.0)
                if let Some(size) = original_size {
                    let mut orders_info = self.active_orders_info.lock().unwrap();
                    orders_info.insert(order_id.clone(), (price, is_up, size, 0.0));

                    // Добавляем открытый ордер в UI
                    let open_order = OpenOrder {
                        order_id: order_id.clone(),
                        is_up,
                        price,
                        filled: 0.0,
                        total: size,
                    };
                    ui::add_open_order(&self.ui_state, open_order);
                }

                // Обновляем UI - добавляем цену в список наших bid prices
                ui::add_our_bid_price(&self.ui_state, is_up, price);

                // Отслеживаем выставленные shares
                if let Some(size) = original_size {
                    let mut port = self.portfolio.lock().unwrap();
                    if is_up {
                        port.up_total_placed += size;
                    } else {
                        port.down_total_placed += size;
                    }
                    drop(port);
                    self.update_ui_portfolio();
                }

                info!("📝 MAKER PLACED: {} {} @ {:.3}",
                    side_str, token_str, price);

                // === ЛОГИКА СОСТОЯНИЯ СТРАТЕГИИ ===
                let mut state = self.trading_state.lock().unwrap();

                // Если мы в WaitingFirstLeg с пустым order_id - обновляем order_id
                if let TradingState::WaitingFirstLeg { order_id: ref mut first_order_id, .. } = *state {
                    if first_order_id.is_empty() {
                        info!("🎯 Первая нога подтверждена: {} @ {:.2}", token_str, price);
                        *first_order_id = order_id.clone();
                    }
                } else if let TradingState::SearchingSecondLeg { first_leg_price, first_leg_is_up, first_leg_size, second_leg_order_id: None } = &*state {
                    // Это размещение второй ноги - сохраняем её order_id
                    info!("📝 Вторая нога подтверждена: {} @ {:.2}", token_str, price);
                    *state = TradingState::SearchingSecondLeg {
                        first_leg_price: *first_leg_price,
                        first_leg_is_up: *first_leg_is_up,
                        first_leg_size: *first_leg_size,
                        second_leg_order_id: Some(order_id.clone()),
                    };
                }
            }
            Some("UPDATE") => {
                // UPDATE = частичное или полное исполнение MAKER ордера
                if let Some(size) = size_matched {
                    // Получаем информацию об ордере и накапливаем исполнение
                    let mut orders_info = self.active_orders_info.lock().unwrap();

                    if let Some((order_price, is_up, original_size, accumulated_filled)) = orders_info.get_mut(&order_id) {
                        // ЗАЩИТА ОТ ПЕРЕУЧЕТА: вычисляем реальную дельту для портфолио
                        let previous_filled = *accumulated_filled;
                        *accumulated_filled += size;

                        // Если accumulated превышает original_size, засчитываем только до лимита
                        let size_for_portfolio = if *accumulated_filled > *original_size {
                            // Переполнение - берем только оставшееся до original_size
                            (*original_size - previous_filled).max(0.0)
                        } else {
                            size
                        };

                        // Сохраняем is_up для использования после освобождения мьютекса
                        let current_is_up = *is_up;
                        let current_accumulated = *accumulated_filled;
                        let current_original_size = *original_size;
                        let current_order_price = *order_price;

                        info!("📊 MAKER PARTIAL FILL: {} {} @ {:.3} | Filled: {:.2}/{:.2}",
                            side_str, token_str, price, *accumulated_filled, *original_size);

                        // Обновляем filled в открытом ордере UI
                        ui::update_open_order_filled(&self.ui_state, &order_id, current_accumulated);

                        // Проверяем, полностью ли исполнен ордер (с погрешностью 0.01)
                        let is_fully_filled = (current_accumulated - current_original_size).abs() < 0.01 || current_accumulated >= current_original_size;

                        if is_fully_filled {
                            // Ордер полностью исполнен - сохраняем данные для удаления
                            let final_price = current_order_price;
                            let final_is_up = current_is_up;

                            // Удаляем из HashMap
                            orders_info.remove(&order_id);
                            drop(orders_info); // Освобождаем мьютекс

                            // Удаляем часики из UI
                            ui::remove_our_bid_price(&self.ui_state, final_is_up, final_price);
                            // Удаляем открытый ордер из UI
                            ui::remove_open_order(&self.ui_state, &order_id);
                            info!("🔔 ОРДЕР ПОЛНОСТЬЮ ИСПОЛНЕН: {} {} @ {:.3}", side_str, token_str, price);

                            // === ЛОГИКА СОСТОЯНИЯ СТРАТЕГИИ ===
                            self.handle_order_fully_filled(&order_id, final_price, final_is_up, current_original_size);
                        } else {
                            drop(orders_info); // Освобождаем мьютекс если ордер еще не полностью исполнен
                        }

                        // Обновляем портфолио только если есть что добавить
                        if size_for_portfolio > 0.0 {
                            let mut port = self.portfolio.lock().unwrap();
                            port.maker_trades += 1;

                            let is_buy = matches!(side, PolySide::Buy);

                            if asset_id == &*self.up_token {
                                if is_buy {
                                    port.up_shares += size_for_portfolio;
                                    port.up_spent += price * size_for_portfolio;
                                } else {
                                    port.up_shares -= size_for_portfolio;
                                    port.up_spent -= price * size_for_portfolio;
                                }
                            } else if asset_id == &*self.down_token {
                                if is_buy {
                                    port.down_shares += size_for_portfolio;
                                    port.down_spent += price * size_for_portfolio;
                                } else {
                                    port.down_shares -= size_for_portfolio;
                                    port.down_spent -= price * size_for_portfolio;
                                }
                            }

                            info!("✅ MAKER FILLED: {} {} @ {:.3} | Size: {:.2} | Cost: ${:.2}",
                                side_str, token_str, price, size_for_portfolio, price * size_for_portfolio);
                            info!("💰 Portfolio: UP {:.1} | DOWN {:.1} | Skew {:.1}",
                                port.up_shares, port.down_shares, port.up_shares - port.down_shares);

                            drop(port);

                            // Добавляем запись в историю торговли (Maker fill)
                            if is_buy {
                                let history_entry = TradeHistoryEntry {
                                    is_up: current_is_up,
                                    shares: size_for_portfolio,
                                    price,
                                    cost: price * size_for_portfolio,
                                    trade_type: TradeType::Maker,
                                    timestamp: Utc::now(),
                                };
                                ui::add_trade_history(&self.ui_state, history_entry);
                            }

                            // Обновляем UI
                            self.update_ui_portfolio();
                        }
                    } else {
                        drop(orders_info); // Освобождаем мьютекс если ордер не найден
                    }
                }
            }
            Some("CANCELLATION") => {
                // Удаляем из активных
                self.remove_order_id(&order_id);

                // Получаем информацию о цене и стороне из нашего хранилища
                let order_info = {
                    let mut orders_info = self.active_orders_info.lock().unwrap();
                    orders_info.remove(&order_id)
                };

                // Обновляем UI - удаляем цену из списка наших bid prices
                if let Some((order_price, is_up, _original_size, _accumulated)) = order_info {
                    ui::remove_our_bid_price(&self.ui_state, is_up, order_price);
                }

                // Удаляем открытый ордер из UI
                ui::remove_open_order(&self.ui_state, &order_id);

                warn!("❌ MAKER CANCELLED: {} {} @ {:.3}",
                    side_str, token_str, price);

                // === ЛОГИКА СОСТОЯНИЯ СТРАТЕГИИ ===
                self.handle_order_cancelled(&order_id);
            }
            _ => {
                // Другие типы событий
            }
        }
    }

    /// Обработка полного заполнения ордера - переходы состояний
    fn handle_order_fully_filled(&self, order_id: &str, filled_price: f64, filled_is_up: bool, filled_size: f64) {
        let mut state = self.trading_state.lock().unwrap();

        match &*state {
            TradingState::WaitingFirstLeg { order_id: first_order_id, .. } => {
                if order_id == first_order_id {
                    // Первая нога заполнена → переходим в SearchingSecondLeg
                    info!("✅ ПЕРВАЯ НОГА ЗАПОЛНЕНА! {} @ {:.2} size={:.2}",
                        if filled_is_up { "UP" } else { "DOWN" }, filled_price, filled_size);

                    *state = TradingState::SearchingSecondLeg {
                        first_leg_price: filled_price,
                        first_leg_is_up: filled_is_up,
                        first_leg_size: filled_size,
                        second_leg_order_id: None,
                    };
                    drop(state);

                    // Размещаем лимитку второй ноги
                    self.place_second_leg(filled_price, filled_is_up, filled_size);
                }
            }
            TradingState::SearchingSecondLeg { second_leg_order_id: Some(second_order_id), .. } => {
                if order_id == second_order_id {
                    // Вторая нога заполнена → возвращаемся в Idle
                    info!("✅ ВТОРАЯ НОГА ЗАПОЛНЕНА! Пара завершена. Возвращаемся в Idle");
                    *state = TradingState::Idle;
                }
            }
            _ => {}
        }
    }

    /// Обработка отмены ордера - переходы состояний
    fn handle_order_cancelled(&self, order_id: &str) {
        let mut state = self.trading_state.lock().unwrap();

        match &*state {
            TradingState::WaitingFirstLeg { order_id: first_order_id, .. } => {
                if order_id == first_order_id {
                    // Первая нога отменена → возвращаемся в Idle
                    info!("⚠️ Первая нога отменена. Возвращаемся в Idle");
                    *state = TradingState::Idle;
                }
            }
            TradingState::SearchingSecondLeg { second_leg_order_id: Some(second_order_id), first_leg_price, first_leg_is_up, first_leg_size } => {
                if order_id == second_order_id {
                    // Вторая нога отменена - продолжаем мониторить хедж без лимитки
                    info!("⚠️ Вторая нога отменена. Продолжаем мониторить хедж");
                    *state = TradingState::SearchingSecondLeg {
                        first_leg_price: *first_leg_price,
                        first_leg_is_up: *first_leg_is_up,
                        first_leg_size: *first_leg_size,
                        second_leg_order_id: None,
                    };
                }
            }
            _ => {}
        }
    }

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

            // Создаем директорию src/redeem если её нет
            let redeem_dir = Path::new("src/redeem");
            if let Err(e) = fs::create_dir_all(redeem_dir) {
                error!("❌ Ошибка создания директории src/redeem: {}", e);
                return;
            }

            // Читаем существующий claim.json или создаем новый массив
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

            // Добавляем новое событие в массив
            claim_events.push(claim_data);

            // Сохраняем обновленный массив в claim.json
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
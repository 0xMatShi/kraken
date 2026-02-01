use crate::models::{
    CumulativePhase, CumulativeState, FirstLeg, MarketPrices, Portfolio, PriceLock, SecondLeg,
    Side, TradePair, Trend,
};
use crate::ui::{self, UiState};
use crate::utils::config::TradingConfig;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use alloy::signers::local::PrivateKeySigner;
use polymarket_client_sdk::auth::Normal;
use polymarket_client_sdk::auth::state::Authenticated;
use polymarket_client_sdk::clob::Client;
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
    pub active_orders_info: Mutex<HashMap<String, (f64, bool, f64, f64)>>, // order_id -> (price, is_up, original_size, accumulated_filled)
    pub our_api_key: Uuid,
    pub config: TradingConfig,
    pub ui_state: UiState,
    pub last_prices: Mutex<Option<MarketPrices>>,
    pub profit_target_reached: Mutex<bool>,

    // === НОВАЯ СТРАТЕГИЯ ===
    /// Предыдущие цены для определения тренда
    pub prev_prices: Mutex<Option<MarketPrices>>,
    /// Активные торговые пары: order_id первой ноги -> TradePair
    pub trade_pairs: Mutex<HashMap<String, TradePair>>,
    /// Mapping: order_id второй ноги -> order_id первой ноги
    pub second_leg_to_first: Mutex<HashMap<String, String>>,
    /// Система блокировки цен
    pub price_lock: Mutex<PriceLock>,
    /// Активные первые ноги по цене для отмены при лучшей цене
    /// Ключ: (side_is_up, price_cents) -> order_id
    pub first_legs_by_price: Mutex<HashMap<(bool, u32), String>>,
    /// Состояние cumulative стратегии (state machine с ZeroPoint как начальным состоянием)
    pub cumulative_state: Mutex<CumulativeState>,
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
        info!("🔧 Инициализация движка: Новая стратегия тренда");
        info!("   Order size: {:.1}", config.size);

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
            // Новая стратегия
            prev_prices: Mutex::new(None),
            trade_pairs: Mutex::new(HashMap::new()),
            second_leg_to_first: Mutex::new(HashMap::new()),
            price_lock: Mutex::new(PriceLock::default()),
            first_legs_by_price: Mutex::new(HashMap::new()),
            cumulative_state: Mutex::new(CumulativeState::default()),
        }
    }

    /// Основной метод стратегии - точка входа для каждого тика рынка
    pub fn process_tick(self: &Arc<Self>, prices: MarketPrices) {
        // Сохраняем последние актуальные цены
        *self.last_prices.lock().unwrap() = Some(prices);

        // Проверяем режим торговли
        let trading_mode = {
            let state = self.ui_state.lock().unwrap();
            state.trading_mode
        };

        match trading_mode {
            ui::TradingMode::Stop | ui::TradingMode::Cancelling | ui::TradingMode::Hedge => {
                // Stop/Cancelling/Hedge режим: ничего не делаем, только обновляем prev_prices
                *self.prev_prices.lock().unwrap() = Some(prices);
                return;
            }
            ui::TradingMode::RealRun => {
                // RealRun режим: реальная торговля
            }
        }

        // Проверяем, прошло ли достаточно времени с начала события
        {
            let state = self.ui_state.lock().unwrap();
            let elapsed = state.event_info.elapsed_seconds();
            let min_elapsed = self.config.seconds_before_start;

            if elapsed < min_elapsed {
                // Слишком рано для торговли, обновляем prev_prices и выходим
                drop(state); // Освобождаем лок перед обновлением prev_prices
                *self.prev_prices.lock().unwrap() = Some(prices);
                return;
            }
        }

        // Проверяем флаг достижения прибыли
        {
            let target_reached = self.profit_target_reached.lock().unwrap();
            if *target_reached {
                return;
            }
        }

        // Проверяем условие прибыльности
        if self.check_profit_target() {
            *self.profit_target_reached.lock().unwrap() = true;

            let engine_clone = Arc::clone(self);
            tokio::spawn(async move {
                engine_clone.cancel_all_orders().await;

                {
                    let mut state = engine_clone.ui_state.lock().unwrap();
                    state.trading_mode = ui::TradingMode::Stop;
                }

                info!("🛑 Бот остановлен. Profit target достигнут.");
            });

            return;
        }

        // Получаем предыдущие цены для определения тренда
        let prev_prices_opt = self.prev_prices.lock().unwrap().clone();

        // Обновляем prev_prices для следующего тика
        *self.prev_prices.lock().unwrap() = Some(prices);

        // Если нет предыдущих цен - это первый тик, пропускаем
        let prev = match prev_prices_opt {
            Some(p) => p,
            None => {
                return;
            }
        };

        // Определяем тренд
        let trend = self.detect_trend(&prev, &prices);

        // Проверяем и отменяем устаревшие ордера (по цене и по тренду)
        self.check_and_cancel_stale_orders(&prices, trend);

        // Фильтруем тренды по legs_strategy
        let legs_strategy = &self.config.legs_strategy;

        // Cumulative стратегия: вызываем даже при trend == None (для middle_tick)
        if legs_strategy == "cumulative" {
            self.process_cumulative(prices, trend);
            return;
        }

        // Для остальных стратегий требуется тренд
        if trend == Trend::None {
            return;
        }

        let should_trade = match legs_strategy.as_str() {
            "strong" => trend == Trend::Strong,
            "weak" => trend == Trend::Weak,
            "both" => true,
            _ => {
                warn!(
                    "⚠️ Неизвестная legs_strategy: {}. Используем 'both'",
                    legs_strategy
                );
                true
            }
        };

        if !should_trade {
            return;
        }

        // Вычисляем параметры размещения первой ноги
        let placement = match self.calculate_first_leg_placement(&prices, trend) {
            Some(p) => p,
            None => return,
        };

        let (side, target_price) = placement;

        // Проверяем блокировку цены
        {
            let lock = self.price_lock.lock().unwrap();
            if lock.is_locked(side, target_price) {
                info!(
                    "🔒 Цена {:.2} на {:?} заблокирована - не размещаем",
                    target_price, side
                );
                return;
            }
        }

        // Проверяем max_balance
        let order_size = self.config.size;
        {
            let port = self.portfolio.lock().unwrap();
            let total_spent = port.up_spent + port.down_spent;
            // Потенциальная стоимость пары: first_leg + second_leg = target_price + (0.99 - target_price) = 0.99
            let pair_cost = 0.99 * order_size;
            if total_spent + pair_cost > self.config.max_balance {
                info!(
                    "⏸️ Max balance достигнут: {:.2} + {:.2} > {:.2}",
                    total_spent, pair_cost, self.config.max_balance
                );
                return;
            }
        }

        // Размещаем первую ногу
        super::streams::place_first_leg(self, side, target_price, order_size);
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

    /// Проверяет условие прибыльности: прибыль с каждой стороны > $5
    pub fn check_profit_target(&self) -> bool {
        let port = self.portfolio.lock().unwrap();

        // Прибыль = shares - spent
        let total_spent = port.up_spent + port.down_spent;
        let up_profit = port.up_shares - total_spent;
        let down_profit = port.down_shares - total_spent;

        // Если обе стороны имеют прибыль > $2
        if up_profit > 2.0 && down_profit > 2.0 {
            info!(
                "🎉 PROFIT TARGET REACHED! UP: ${:.2} | DOWN: ${:.2}",
                up_profit, down_profit
            );
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
                self.trade_pairs.lock().unwrap().clear();
                self.second_leg_to_first.lock().unwrap().clear();
                self.first_legs_by_price.lock().unwrap().clear();
                *self.cumulative_state.lock().unwrap() = CumulativeState::default();
                // price_lock не очищаем - пусть цены остаются заблокированными
            }
            Err(e) => {
                warn!("❌ Ошибка отмены ордеров: {}", e);
            }
        }
    }

    /// Определяет тренд на основе изменения цен
    ///
    /// Тренд сильной стороны: bb слабой стороны уменьшился (сильная становится еще дороже)
    /// Тренд слабой стороны: bb сильной стороны уменьшился (слабая догоняет)
    pub fn detect_trend(&self, prev: &MarketPrices, current: &MarketPrices) -> Trend {
        let strong_side = current.strong_side();

        match strong_side {
            Side::Up => {
                // UP сильная сторона (up_bid > down_bid)
                // Тренд сильной стороны: down_bid уменьшился
                if current.down_bid < prev.down_bid {
                    return Trend::Strong;
                }
                // Тренд слабой стороны: up_bid уменьшился
                if current.up_bid < prev.up_bid {
                    return Trend::Weak;
                }
            }
            Side::Down => {
                // DOWN сильная сторона (down_bid > up_bid)
                // Тренд сильной стороны: up_bid уменьшился
                if current.up_bid < prev.up_bid {
                    return Trend::Strong;
                }
                // Тренд слабой стороны: down_bid уменьшился
                if current.down_bid < prev.down_bid {
                    return Trend::Weak;
                }
            }
        }

        Trend::None
    }

    /// Вычисляет цену для размещения первой ноги
    /// Возвращает (сторона размещения, цена) или None если условия не выполнены
    ///
    /// Универсальная логика для обоих типов трендов:
    /// - Trend::Strong: размещаем на сильной стороне
    /// - Trend::Weak: размещаем на слабой стороне
    pub fn calculate_first_leg_placement(
        &self,
        prices: &MarketPrices,
        trend: Trend,
    ) -> Option<(Side, f64)> {
        // Только если есть тренд
        if trend == Trend::None {
            return None;
        }

        let spread_cents = prices.spread_cents();

        // Спред 4+ цента - ничего не делаем
        if spread_cents >= 4 {
            info!("⏸️ Спред {} центов >= 4 - не размещаем", spread_cents);
            return None;
        }

        // Спред 1 цент (нормальный) - ничего не делаем
        if spread_cents <= 1 {
            return None;
        }

        // Обрабатываем спред 2-3 цента
        let strong_side = prices.strong_side();
        let weak_side = prices.weak_side();

        // Определяем сторону размещения в зависимости от типа тренда
        let (placement_side, opposite_side) = match trend {
            Trend::Strong => (strong_side, weak_side),
            Trend::Weak => (weak_side, strong_side),
            Trend::None => return None,
        };

        let opposite_bb = prices.bid_for_side(opposite_side);

        // Цена = 0.99 - opposite_bb, чтобы сумма была 0.99
        let target_price = Self::round_price(0.99 - opposite_bb);

        info!(
            "📈 Тренд {:?} на стороне {:?}: opposite_bb={:.2}, target_price={:.2}",
            trend, placement_side, opposite_bb, target_price
        );

        Some((placement_side, target_price))
    }

    /// Проверяет и отменяет устаревшие ордера
    ///
    /// Для обычных ордеров: отменяем если цена ниже текущего best_bid
    /// Для cumulative:
    /// - Первая нога: отменяем если цена ниже best_bid ИЛИ trend == Strong
    /// - Вторая нога: отменяем если цена ниже best_bid ИЛИ trend == Weak
    fn check_and_cancel_stale_orders(self: &Arc<Self>, prices: &MarketPrices, _trend: Trend) {
        // Текущие лучшие биды для каждой стороны
        let up_best_bid_cents = (prices.up_bid * 100.0).round() as u32;
        let down_best_bid_cents = (prices.down_bid * 100.0).round() as u32;

        let orders_to_cancel: Vec<(String, f64)> = {
            let first_legs = self.first_legs_by_price.lock().unwrap();
            first_legs
                .iter()
                .filter(|((is_up, price_cents), _)| {
                    // Определяем текущий лучший бид для этой стороны
                    let current_best_bid_cents = if *is_up {
                        up_best_bid_cents
                    } else {
                        down_best_bid_cents
                    };
                    // Отменяем, если цена ордера ниже текущего лучшего бида
                    *price_cents < current_best_bid_cents
                })
                .map(|((_, price_cents), order_id)| (order_id.clone(), *price_cents as f64 / 100.0))
                .collect()
        };

        if !orders_to_cancel.is_empty() {
            info!(
                "🗑️ Отменяем {} устаревших ордеров (цена ниже текущего бида)",
                orders_to_cancel.len()
            );
            let order_ids: Vec<String> =
                orders_to_cancel.iter().map(|(id, _)| id.clone()).collect();
            super::streams::cancel_orders(self, order_ids);
        }

        // Проверяем cumulative ордера
        let mut cum_state = self.cumulative_state.lock().unwrap();
        let phase = cum_state.phase;

        match phase {
            CumulativePhase::FirstLegPlaced => {
                // Отменяем ордера первой ноги, цена которых ниже текущего best_bid
                let best_bid = prices.bid_for_side(cum_state.first_leg_side);

                // Собираем ордера для отмены (цена ордера < best_bid)
                let stale_orders: Vec<String> = cum_state
                    .first_leg_orders
                    .iter()
                    .filter(|(_, order_price)| **order_price < best_bid)
                    .map(|(order_id, _)| order_id.clone())
                    .collect();

                if !stale_orders.is_empty() {
                    info!(
                        "🗑️ [Cumulative] Отменяем {} устаревших ордеров первой ноги (price < best_bid {:.2})",
                        stale_orders.len(),
                        best_bid
                    );
                    // Удаляем из HashMap
                    for order_id in &stale_orders {
                        cum_state.first_leg_orders.remove(order_id);
                    }
                    // Сбрасываем placed_price если все ордера отменены
                    if cum_state.first_leg_orders.is_empty() {
                        cum_state.first_leg_placed_price = None;
                    }
                    cum_state.pending_first_leg_orders = 0;
                    drop(cum_state);
                    super::streams::cancel_orders(self, stale_orders);
                    return;
                }
            }
            CumulativePhase::SecondLegPlaced => {
                // Отменяем ордера второй ноги, цена которых ниже текущего best_bid слабой стороны
                let weak_side = cum_state.first_leg_side.opposite();
                let best_bid = prices.bid_for_side(weak_side);

                // Собираем ордера для отмены (цена ордера < best_bid)
                let stale_orders: Vec<String> = cum_state
                    .second_leg_orders
                    .iter()
                    .filter(|(_, order_price)| **order_price < best_bid)
                    .map(|(order_id, _)| order_id.clone())
                    .collect();

                if !stale_orders.is_empty() {
                    info!(
                        "🗑️ [Cumulative] Отменяем {} устаревших ордеров второй ноги (price < best_bid {:.2})",
                        stale_orders.len(),
                        best_bid
                    );
                    // Удаляем из HashMap
                    for order_id in &stale_orders {
                        cum_state.second_leg_orders.remove(order_id);
                    }
                    // Сбрасываем placed_price если все ордера отменены
                    if cum_state.second_leg_orders.is_empty() {
                        cum_state.second_leg_placed_price = None;
                    }
                    cum_state.pending_second_leg_orders = 0;
                    drop(cum_state);
                    super::streams::cancel_orders(self, stale_orders);
                    return;
                }
            }
            CumulativePhase::Middle => {
                // В фазе Middle отменяем оставшиеся ордера первой ноги
                // Это нужно когда при переразмещении первой ноги старые ордера не были отменены
                if !cum_state.first_leg_orders.is_empty() {
                    info!(
                        "🗑️ [Cumulative] Отменяем {} оставшихся ордеров первой ноги (фаза Middle)",
                        cum_state.first_leg_orders.len()
                    );
                    let orders: Vec<String> = cum_state.first_leg_orders.keys().cloned().collect();
                    drop(cum_state);
                    super::streams::cancel_orders(self, orders);
                }
            }
            CumulativePhase::ZeroPoint => {
                // В ZeroPoint отменяем оставшиеся ордера второй ноги от предыдущего цикла
                if !cum_state.second_leg_orders.is_empty() {
                    info!(
                        "🗑️ [Cumulative] Отменяем {} оставшихся ордеров второй ноги (фаза ZeroPoint)",
                        cum_state.second_leg_orders.len()
                    );
                    let orders: Vec<String> = cum_state.second_leg_orders.keys().cloned().collect();
                    drop(cum_state);
                    super::streams::cancel_orders(self, orders);
                }
            }
            // В Beginning не проверяем - ещё нет размещённых ордеров
            CumulativePhase::Beginning => {}
        }
    }

    /// Пытается переразместить вторую ногу с повышением цены на 0.01
    /// Вызывается из таймера после размещения второй ноги
    fn try_reprice_second_leg(
        self: &Arc<Self>,
        first_leg_order_id: &str,
        second_leg_order_id: &str,
    ) {
        // Проверяем существует ли еще эта пара и вторая нога
        let reprice_params = {
            let pairs = self.trade_pairs.lock().unwrap();
            if let Some(pair) = pairs.get(first_leg_order_id) {
                if let Some(second_leg) = &pair.second_leg {
                    // Проверяем что это та же самая нога (по order_id)
                    if second_leg.order_id == second_leg_order_id {
                        // Проверяем что не исполнена полностью
                        if second_leg.filled < second_leg.size {
                            Some((
                                second_leg.side,
                                second_leg.price,
                                second_leg.size,
                                pair.first_leg.price,
                                second_leg.timer_interval_secs,
                            ))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            }
        };

        let (side, old_price, size, first_leg_price, current_interval) = match reprice_params {
            Some(params) => params,
            None => return, // Нога уже исполнена или отменена
        };

        // Вычисляем новый интервал таймера (уменьшаем на 1, минимум 2)
        let new_interval = if current_interval > 2 {
            current_interval - 2
        } else {
            2
        };

        // Получаем текущие рыночные цены
        let current_prices = {
            let prices_opt = self.last_prices.lock().unwrap();
            match *prices_opt {
                Some(prices) => prices,
                None => {
                    warn!("❌ Не удалось получить текущие цены для переразмещения второй ноги");
                    return;
                }
            }
        };

        // Определяем текущий best_bid слабой стороны
        let current_best_bid = current_prices.bid_for_side(side);

        // Если цена второй ноги == текущий best_bid, то не переразмещаем
        if (old_price - current_best_bid).abs() == 0.0 {
            info!(
                "✅ Вторая нога {} уже на best_bid {:.2} - перезапускаем таймер на {}s",
                second_leg_order_id, current_best_bid, new_interval
            );

            // Запускаем новый таймер с уменьшенным интервалом
            let engine_clone = Arc::clone(self);
            let first_id = first_leg_order_id.to_string();
            let second_id = second_leg_order_id.to_string();

            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(new_interval)).await;
                engine_clone.try_reprice_second_leg(&first_id, &second_id);
            });

            return;
        }

        // Рынок изменился - переразмещаем по новому best_bid
        let new_price = Self::round_price(old_price + 0.02);

        info!(
            "🔄 Переразмещаем вторую ногу {}: {:?} {:.2} → {:.2} | новый таймер: {}s",
            second_leg_order_id, side, old_price, new_price, new_interval
        );

        // Отменяем старый ордер
        super::streams::cancel_order(self, second_leg_order_id.to_string());

        // Удаляем из mapping
        {
            let mut mapping = self.second_leg_to_first.lock().unwrap();
            mapping.remove(second_leg_order_id);
        }

        // Удаляем вторую ногу из пары
        {
            let mut pairs = self.trade_pairs.lock().unwrap();
            if let Some(pair) = pairs.get_mut(first_leg_order_id) {
                pair.second_leg = None;
            }
        }

        // Размещаем новую вторую ногу с повышенной ценой и уменьшенным интервалом таймера
        super::streams::place_second_leg(
            self,
            first_leg_order_id.to_string(),
            side,
            new_price,
            size,
            first_leg_price,
            new_interval,
        );
    }

    /// Регистрирует первую ногу после получения order_id
    pub fn register_first_leg(&self, order_id: String, side: Side, price: f64, size: f64) {
        let is_up = matches!(side, Side::Up);
        let price_cents = (price * 100.0).round() as u32;

        // Блокируем цену
        {
            let mut lock = self.price_lock.lock().unwrap();
            lock.lock(side, price);
        }

        // Добавляем в first_legs_by_price
        {
            let mut first_legs = self.first_legs_by_price.lock().unwrap();
            first_legs.insert((is_up, price_cents), order_id.clone());
        }

        // Создаем TradePair
        let first_leg = FirstLeg {
            order_id: order_id.clone(),
            price,
            size,
            side,
            filled: 0.0,
        };

        let trade_pair = TradePair {
            first_leg,
            second_leg: None,
        };

        {
            let mut pairs = self.trade_pairs.lock().unwrap();
            pairs.insert(order_id.clone(), trade_pair);
        }

        info!(
            "📝 Первая нога зарегистрирована: {} {:?} @ {:.2}",
            order_id, side, price
        );
    }

    /// Обрабатывает полное исполнение первой ноги и размещает вторую
    /// Вторая нога размещается по текущему best_bid слабой стороны
    pub fn on_first_leg_filled(self: &Arc<Self>, order_id: &str) {
        let trade_pair = {
            let pairs = self.trade_pairs.lock().unwrap();
            pairs.get(order_id).cloned()
        };

        if let Some(pair) = trade_pair {
            let first_leg = &pair.first_leg;
            let second_leg_side = first_leg.side.opposite();

            // Берем текущий best_bid слабой стороны
            let second_leg_price = 0.98 - first_leg.price;
            let second_leg_size = first_leg.size;

            info!(
                "🎯 Первая нога {} исполнена! Размещаем вторую ногу: {:?} @ {:.2} (текущий best_bid)",
                order_id, second_leg_side, second_leg_price
            );

            // Удаляем из first_legs_by_price
            {
                let is_up = matches!(first_leg.side, Side::Up);
                let price_cents = (first_leg.price * 100.0).round() as u32;
                let mut first_legs = self.first_legs_by_price.lock().unwrap();
                first_legs.remove(&(is_up, price_cents));
            }

            // Размещаем вторую ногу с начальным таймером 9 секунд
            super::streams::place_second_leg(
                self,
                order_id.to_string(),
                second_leg_side,
                second_leg_price,
                second_leg_size,
                first_leg.price,
                10, // Начальный интервал таймера
            );
        }
    }

    /// Регистрирует вторую ногу после получения order_id
    /// Запускает таймер переразмещения с указанным интервалом
    pub fn register_second_leg(
        self: &Arc<Self>,
        first_leg_order_id: &str,
        second_leg_order_id: String,
        side: Side,
        price: f64,
        size: f64,
        first_leg_price: f64,
        timer_interval_secs: u64,
    ) {
        let second_leg = SecondLeg {
            order_id: second_leg_order_id.clone(),
            price,
            size,
            side,
            filled: 0.0,
            first_leg_price,
            last_placed: Instant::now(),
            timer_interval_secs,
        };

        // Обновляем TradePair
        {
            let mut pairs = self.trade_pairs.lock().unwrap();
            if let Some(pair) = pairs.get_mut(first_leg_order_id) {
                pair.second_leg = Some(second_leg);
            }
        }

        // Добавляем mapping second -> first
        {
            let mut mapping = self.second_leg_to_first.lock().unwrap();
            mapping.insert(second_leg_order_id.clone(), first_leg_order_id.to_string());
        }

        info!(
            "📝 Вторая нога зарегистрирована: {} {:?} @ {:.2} | таймер: {}s",
            second_leg_order_id, side, price, timer_interval_secs
        );

        // Запускаем таймер переразмещения с указанным интервалом
        let engine_clone = Arc::clone(self);
        let first_id = first_leg_order_id.to_string();
        let second_id = second_leg_order_id.clone();

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(timer_interval_secs)).await;
            engine_clone.try_reprice_second_leg(&first_id, &second_id);
        });
    }

    /// Обрабатывает полное исполнение второй ноги - освобождает цену
    pub fn on_second_leg_filled(&self, second_leg_order_id: &str) {
        // Находим first_leg_order_id
        let first_leg_order_id = {
            let mapping = self.second_leg_to_first.lock().unwrap();
            mapping.get(second_leg_order_id).cloned()
        };

        if let Some(first_id) = first_leg_order_id {
            // Получаем TradePair для извлечения цены первой ноги
            let first_leg_price = {
                let pairs = self.trade_pairs.lock().unwrap();
                pairs
                    .get(&first_id)
                    .map(|p| (p.first_leg.price, p.first_leg.side))
            };

            if let Some((price, side)) = first_leg_price {
                // Освобождаем цену
                {
                    let mut lock = self.price_lock.lock().unwrap();
                    lock.unlock(side, price);
                }

                info!(
                    "🔓 Цена {:.2} на {:?} разблокирована - пара завершена",
                    price, side
                );
            }

            // Очищаем структуры
            {
                let mut pairs = self.trade_pairs.lock().unwrap();
                pairs.remove(&first_id);
            }
            {
                let mut mapping = self.second_leg_to_first.lock().unwrap();
                mapping.remove(second_leg_order_id);
            }
        }
    }

    /// Обрабатывает отмену первой ноги
    pub fn on_first_leg_cancelled(&self, order_id: &str) {
        let trade_pair = {
            let mut pairs = self.trade_pairs.lock().unwrap();
            pairs.remove(order_id)
        };

        if let Some(pair) = trade_pair {
            let first_leg = &pair.first_leg;

            // Удаляем из first_legs_by_price
            {
                let is_up = matches!(first_leg.side, Side::Up);
                let price_cents = (first_leg.price * 100.0).round() as u32;
                let mut first_legs = self.first_legs_by_price.lock().unwrap();
                first_legs.remove(&(is_up, price_cents));
            }

            // Освобождаем цену
            {
                let mut lock = self.price_lock.lock().unwrap();
                lock.unlock(first_leg.side, first_leg.price);
            }

            info!(
                "🔓 Первая нога {} отменена, цена {:.2} разблокирована",
                order_id, first_leg.price
            );
        }
    }

    /// Вычисляет размеры ордеров для батчевого размещения
    ///
    /// Разбивает remaining на порции по unit_size.
    /// Остаток <= 5.0 прибавляется к последнему ордеру.
    pub fn calculate_order_sizes(remaining: f64, unit_size: f64) -> Vec<f64> {
        if remaining <= 5.0 {
            return vec![];
        }

        let full_count = (remaining / unit_size).floor() as usize;
        let remainder = Self::round_price(remaining - full_count as f64 * unit_size);

        if remainder == 0.0 {
            // Точно делится
            return vec![unit_size; full_count];
        }

        if full_count == 0 {
            // Только остаток
            return vec![remainder];
        }

        if remainder > 5.0 {
            // Остаток достаточно большой - отдельный ордер
            let mut sizes = vec![unit_size; full_count];
            sizes.push(remainder);
            return sizes;
        }

        // Остаток <= 5.0 - прибавляем к последнему
        if full_count >= 2 {
            let mut sizes = vec![unit_size; full_count - 1];
            sizes.push(Self::round_price(unit_size + remainder));
            sizes
        } else {
            // full_count == 1
            vec![Self::round_price(unit_size + remainder)]
        }
    }

    /// Финализация сессии - генерация отчета
    pub fn finalize(&self, final_prices: &MarketPrices) {
        let port = self.portfolio.lock().unwrap();
        let winner = if final_prices.up_bid > 0.5 {
            Side::Up
        } else {
            Side::Down
        };
        let winning_shares = if winner == Side::Up {
            port.up_shares
        } else {
            port.down_shares
        };
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

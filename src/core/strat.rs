use std::sync::{Arc, Mutex};
use std::collections::{HashSet, HashMap};
use crate::models::{Portfolio, Side, MarketPrices, Trend, FirstLeg, SecondLeg, TradePair, PriceLock};
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

    /// Проверяет условие прибыльности: прибыль с каждой стороны > $5
    pub fn check_profit_target(&self) -> bool {
        let port = self.portfolio.lock().unwrap();

        // Прибыль = shares - spent
        let total_spent = port.up_spent + port.down_spent;
        let up_profit = port.up_shares - total_spent;
        let down_profit = port.down_shares - total_spent;

        info!("💰 Profit Check: UP profit: ${:.2} | DOWN profit: ${:.2}", up_profit, down_profit);

        // Если обе стороны имеют прибыль > $5
        if up_profit > 3.0 && down_profit > 3.0 {
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
                self.trade_pairs.lock().unwrap().clear();
                self.second_leg_to_first.lock().unwrap().clear();
                self.first_legs_by_price.lock().unwrap().clear();
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
    pub fn detect_trend(&self, prev: &MarketPrices, current: &MarketPrices) -> Trend {
        let strong_side = current.strong_side();
        
        match strong_side {
            Side::Up => {
                // UP сильная сторона (up_bid > 0.5)
                // Тренд сильной стороны: down_bid уменьшился
                if current.down_bid < prev.down_bid {
                    return Trend::Strong;
                }
            }
            Side::Down => {
                // DOWN сильная сторона (down_bid > 0.5)
                // Тренд сильной стороны: up_bid уменьшился
                if current.up_bid < prev.up_bid {
                    return Trend::Strong;
                }
            }
        }
        
        Trend::None
    }

    /// Вычисляет цену для размещения первой ноги
    /// Возвращает (сторона размещения, цена) или None если условия не выполнены
    /// 
    /// Размещение только на сильной стороне при тренде сильной стороны
    pub fn calculate_first_leg_placement(&self, prices: &MarketPrices, trend: Trend) -> Option<(Side, f64)> {
        // Только тренд сильной стороны
        if trend != Trend::Strong {
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
        
        // Спред 2-3 цента - размещаем на сильной стороне
        let strong_side = prices.strong_side();
        let weak_side = prices.weak_side();
        let weak_bb = prices.bid_for_side(weak_side);
        
        // Цена = 0.99 - weak_bb, чтобы сумма была 0.99
        let target_price = Self::round_price(0.99 - weak_bb);
        
        info!("📈 Тренд СИЛЬНОЙ стороны ({:?}): weak_bb={:.2}, target_price={:.2}",
            strong_side, weak_bb, target_price);
        
        Some((strong_side, target_price))
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
            ui::TradingMode::Stop | ui::TradingMode::Cancelling => {
                // Stop/Cancelling режим: ничего не делаем, только обновляем prev_prices
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
                info!("⏳ Ждем начала торговли: {} / {} сек", elapsed, min_elapsed);
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

        // Логируем текущее состояние стакана
        let up_bid = prices.up_bid;
        let down_bid = prices.down_bid;
        let spread = prices.spread_cents();
        
        info!("📊 Тик: UP bid {:.3} | DOWN bid {:.3} | Spread: {} центов",
            up_bid, down_bid, spread);

        // Получаем предыдущие цены для определения тренда
        let prev_prices_opt = self.prev_prices.lock().unwrap().clone();
        
        // Обновляем prev_prices для следующего тика
        *self.prev_prices.lock().unwrap() = Some(prices);
        
        // Если нет предыдущих цен - это первый тик, пропускаем
        let prev = match prev_prices_opt {
            Some(p) => p,
            None => {
                info!("⏭️ Первый тик - пропускаем, ждем следующий для определения тренда");
                return;
            }
        };

        // Определяем тренд
        let trend = self.detect_trend(&prev, &prices);
        
        if trend == Trend::None {
            info!("⏸️ Нет тренда - не размещаем");
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
                info!("🔒 Цена {:.2} на {:?} заблокирована - не размещаем", target_price, side);
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
                info!("⏸️ Max balance достигнут: {:.2} + {:.2} > {:.2}",
                    total_spent, pair_cost, self.config.max_balance);
                return;
            }
        }

        // Проверяем, есть ли уже первая нога по более низкой цене на этой стороне
        // Если есть - нужно отменить её
        self.check_and_cancel_lower_price_orders(side, target_price);

        // Размещаем первую ногу
        super::streams::place_first_leg(self, side, target_price, order_size);
    }

    /// Проверяет и отменяет первые ноги с ценой ниже целевой
    fn check_and_cancel_lower_price_orders(self: &Arc<Self>, side: Side, target_price: f64) {
        let target_cents = (target_price * 100.0).round() as u32;
        let is_up = matches!(side, Side::Up);
        
        let orders_to_cancel: Vec<String> = {
            let first_legs = self.first_legs_by_price.lock().unwrap();
            first_legs.iter()
                .filter(|((side_is_up, price_cents), _)| {
                    *side_is_up == is_up && *price_cents < target_cents
                })
                .map(|(_, order_id)| order_id.clone())
                .collect()
        };

        for order_id in orders_to_cancel {
            info!("🗑️ Отменяем первую ногу {} (цена ниже {:.2})", order_id, target_price);
            super::streams::cancel_order(self, order_id);
        }
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
        
        info!("📝 Первая нога зарегистрирована: {} {:?} @ {:.2}", order_id, side, price);
    }

    /// Обрабатывает полное исполнение первой ноги и размещает вторую
    pub fn on_first_leg_filled(self: &Arc<Self>, order_id: &str) {
        let trade_pair = {
            let pairs = self.trade_pairs.lock().unwrap();
            pairs.get(order_id).cloned()
        };
        
        if let Some(pair) = trade_pair {
            let first_leg = &pair.first_leg;
            let second_leg_side = first_leg.side.opposite();
            let second_leg_price = Self::round_price(0.99 - first_leg.price);
            let second_leg_size = first_leg.size;
            
            info!("🎯 Первая нога {} исполнена! Размещаем вторую ногу: {:?} @ {:.2}",
                order_id, second_leg_side, second_leg_price);
            
            // Удаляем из first_legs_by_price
            {
                let is_up = matches!(first_leg.side, Side::Up);
                let price_cents = (first_leg.price * 100.0).round() as u32;
                let mut first_legs = self.first_legs_by_price.lock().unwrap();
                first_legs.remove(&(is_up, price_cents));
            }
            
            // Размещаем вторую ногу
            super::streams::place_second_leg(
                self,
                order_id.to_string(),
                second_leg_side,
                second_leg_price,
                second_leg_size,
                first_leg.price,
            );
        }
    }

    /// Регистрирует вторую ногу после получения order_id
    pub fn register_second_leg(
        &self,
        first_leg_order_id: &str,
        second_leg_order_id: String,
        side: Side,
        price: f64,
        size: f64,
        first_leg_price: f64,
    ) {
        let second_leg = SecondLeg {
            order_id: second_leg_order_id.clone(),
            price,
            size,
            side,
            filled: 0.0,
            first_leg_price,
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
        
        info!("📝 Вторая нога зарегистрирована: {} {:?} @ {:.2}", second_leg_order_id, side, price);
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
                pairs.get(&first_id).map(|p| (p.first_leg.price, p.first_leg.side))
            };
            
            if let Some((price, side)) = first_leg_price {
                // Освобождаем цену
                {
                    let mut lock = self.price_lock.lock().unwrap();
                    lock.unlock(side, price);
                }
                
                info!("🔓 Цена {:.2} на {:?} разблокирована - пара завершена", price, side);
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
            
            info!("🔓 Первая нога {} отменена, цена {:.2} разблокирована", order_id, first_leg.price);
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

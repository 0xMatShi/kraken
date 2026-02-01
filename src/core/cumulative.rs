use super::strat::RealEngine;
use crate::models::{CumulativePhase, CumulativeState, MarketPrices, Side, Trend};
use std::sync::Arc;
use tracing::info;

impl RealEngine {
    /// Основной тик cumulative стратегии
    ///
    /// State machine:
    /// ZeroPoint → Beginning → FirstLegPlaced → Middle → SecondLegPlaced → ZeroPoint
    ///
    /// Роутинг по фазе, тренду и спреду:
    /// - ZeroPoint + Strong + spread 2-3: start_cumulative_tick
    /// - Beginning: все тики скипаются
    /// - FirstLegPlaced + Strong + spread 2-3: first_leg_replacement_tick
    /// - Middle + (Strong|Weak): middle_tick
    /// - SecondLegPlaced + (Strong|Weak): second_leg_replacement_tick
    pub fn process_cumulative(self: &Arc<Self>, prices: MarketPrices, trend: Trend) {
        let phase = {
            let state = self.cumulative_state.lock().unwrap();
            state.phase
        };

        let spread = prices.spread_cents();
        let has_good_spread = spread >= 2 && spread < 5;

        match phase {
            CumulativePhase::ZeroPoint => {
                // Начинаем цикл только при тренде Strong + спред 2-4
                if trend == Trend::Strong && has_good_spread {
                    self.start_tick(prices);
                }
            }
            CumulativePhase::Beginning => {
                // Все тики скипаются пока идёт размещение первой ноги
                // Переход в FirstLegPlaced произойдёт когда все pending ордера подтвердятся
            }
            CumulativePhase::FirstLegPlaced => {
                // Переразмещаем первую ногу только при тренде Strong
                if trend == Trend::Strong && has_good_spread {
                    self.first_leg_replacement_tick(prices);
                }
            }
            CumulativePhase::Middle => {
                // Размещаем вторую ногу при любом тренде (Strong, Weak или None)
                self.middle_tick(prices, trend);
            }
            CumulativePhase::SecondLegPlaced => {
                // Переразмещаем вторую ногу при любом тренде
                self.second_leg_replacement_tick(prices, trend);
            }
        }
    }

    /// Фаза ZeroPoint: ищем условия для начала цикла
    ///
    /// Условия входа (проверяются в process_cumulative):
    /// - Trend::Strong
    /// - Spread 2-3 цента
    ///
    /// Здесь проверяем только max_balance
    /// Также добавляем компенсацию skew к первой ноге если перекос не в её пользу
    fn start_tick(self: &Arc<Self>, prices: MarketPrices) {
        // Получаем skew из portfolio для компенсации
        let skew_compensation = {
            let port = self.portfolio.lock().unwrap();
            let total_spent = port.up_spent + port.down_spent;
            let potential_cost = 0.99 * self.config.max_size_side;
            if total_spent + potential_cost > self.config.max_balance {
                info!(
                    "⏸️ [Cumulative] Max balance достигнут: {:.2} + {:.2} > {:.2}",
                    total_spent, potential_cost, self.config.max_balance
                );
                return;
            }
            // skew = up_shares - down_shares
            // Если skew < 0 → больше DOWN, нужно компенсировать при UP первой ноге
            // Если skew > 0 → больше UP, нужно компенсировать при DOWN первой ноге
            port.up_shares - port.down_shares
        };

        // ВАЖНО: Сначала переходим в Beginning чтобы заблокировать следующие тики
        let (strong_side, target_price, sizes) = {
            let mut cum_state = self.cumulative_state.lock().unwrap();

            // Double-check что мы всё ещё в ZeroPoint
            if cum_state.phase != CumulativePhase::ZeroPoint {
                return;
            }

            let strong_side = prices.strong_side();
            let weak_bb = prices.bid_for_side(prices.weak_side());
            let target_price = Self::round_price(0.99 - weak_bb);

            // Базовый размер без компенсации (для второй ноги)
            let base_target_size = self.config.max_size_side;

            // Добавляем компенсацию skew если перекос не в пользу первой ноги
            let compensation = match strong_side {
                Side::Up if skew_compensation < 0.0 => -skew_compensation, // больше DOWN, компенсируем UP
                Side::Down if skew_compensation > 0.0 => skew_compensation, // больше UP, компенсируем DOWN
                _ => 0.0,
            };
            let target_size = base_target_size + compensation;

            let sizes = Self::calculate_order_sizes(target_size, self.config.size);
            if sizes.is_empty() {
                return;
            }

            // Переходим в Beginning и устанавливаем pending_first_leg_orders
            cum_state.phase = CumulativePhase::Beginning;
            cum_state.first_leg_side = strong_side;
            cum_state.first_leg_target_size = target_size;
            cum_state.base_target_size = base_target_size;
            cum_state.first_leg_placed_price = Some(target_price);
            cum_state.pending_first_leg_orders = sizes.len() as u32;

            if compensation > 0.0 {
                info!(
                    "🚀 [Cumulative] ZeroPoint → Beginning: {:?} @ {:.2} | {} ордеров | target={:.2} (base={:.2} + skew_comp={:.2})",
                    strong_side,
                    target_price,
                    sizes.len(),
                    target_size,
                    base_target_size,
                    compensation
                );
            } else {
                info!(
                    "🚀 [Cumulative] ZeroPoint → Beginning: {:?} @ {:.2} | {} ордеров | target_size={:.2}",
                    strong_side,
                    target_price,
                    sizes.len(),
                    target_size
                );
            }

            (strong_side, target_price, sizes)
        };

        // Размещаем первую ногу (state уже в Beginning)
        super::streams::place_cumulative_first_leg(self, strong_side, target_price, sizes);
    }

    /// Фаза FirstLegPlaced: мониторим тренд strong для переразмещения первой ноги
    ///
    /// Условия (проверяются в process_cumulative):
    /// - Trend::Strong
    ///
    /// При выполнении условий:
    /// - Переразмещаем по новой цене (0.99 - weak_bb)
    /// - Старые ордера отменятся на следующем тике через check_and_cancel_stale_orders
    fn first_leg_replacement_tick(self: &Arc<Self>, prices: MarketPrices) {
        // Вычисляем целевую цену и размещаем
        let weak_bb = prices.bid_for_side(prices.weak_side());
        let target_price = Self::round_price(0.99 - weak_bb);

        let (strong_side, sizes) = {
            let mut cum_state = self.cumulative_state.lock().unwrap();

            if cum_state.pending_first_leg_orders > 0 {
                return;
            }

            let remaining = cum_state.first_leg_target_size - cum_state.first_leg_filled;
            let sizes = Self::calculate_order_sizes(remaining, self.config.size);

            if sizes.is_empty() {
                return;
            }

            // Устанавливаем pending для новых ордеров
            cum_state.pending_first_leg_orders = sizes.len() as u32;
            cum_state.first_leg_placed_price = Some(target_price);

            info!(
                "🔄 [Cumulative] Переразмещаем первую ногу: → {:.2} | {} ордеров | remaining={:.2}",
                target_price,
                sizes.len(),
                remaining
            );

            (cum_state.first_leg_side, sizes)
        };

        super::streams::place_cumulative_first_leg(self, strong_side, target_price, sizes);
    }

    /// Фаза Middle: ждём условий для размещения второй ноги
    ///
    /// Три случая размещения:
    /// 1. Тренд Strong → размещаем по 0.99 - strong_bb
    /// 2. Тренд Weak → размещаем по 0.99 - strong_bb
    /// 3. На best_bid слабой стороны акций < max_size_side → присоединяемся к best_bid
    fn middle_tick(self: &Arc<Self>, prices: MarketPrices, trend: Trend) {
        let (weak_side, strong_side) = {
            let cum_state = self.cumulative_state.lock().unwrap();
            (
                cum_state.first_leg_side.opposite(),
                cum_state.first_leg_side,
            )
        };

        let weak_bb = prices.bid_for_side(weak_side);
        let weak_bb_size = prices.bid_size_for_side(weak_side);
        let strong_bb = prices.bid_for_side(strong_side);
        let spread = prices.spread_cents();
        let has_good_spread = spread >= 2 && spread < 4;

        // Определяем цену для второй ноги
        let target_price = if weak_bb_size <= 100.0 {
            // Случай 2: небольшая очередь - присоединяемся к best_bid
            weak_bb
        } else if trend == Trend::Strong || trend == Trend::Weak && has_good_spread {
            // Случай 1: тренд + спред - размещаем лимитку
            Self::round_price(0.99 - strong_bb)
        } else {
            // Нет подходящих условий
            return;
        };

        let (should_place, sizes) = {
            let mut cum_state = self.cumulative_state.lock().unwrap();

            // Double-check что мы в Middle
            if cum_state.phase != CumulativePhase::Middle {
                return;
            }

            // Вторая нога = min(first_leg_filled, base_target_size) - second_leg_filled
            // Используем base_target_size (без компенсации skew) чтобы не накапливать перекос
            let second_leg_target = cum_state.base_target_size.min(cum_state.first_leg_filled);
            let remaining = second_leg_target - cum_state.second_leg_filled;
            let sizes = Self::calculate_order_sizes(remaining, self.config.size);

            if sizes.is_empty() {
                return;
            }

            // Переходим в SecondLegPlaced
            cum_state.phase = CumulativePhase::SecondLegPlaced;
            cum_state.second_leg_placed_price = Some(target_price);
            cum_state.pending_second_leg_orders = sizes.len() as u32;

            info!(
                "📦 [Cumulative] Middle → SecondLegPlaced: {:?} @ {:.2} | {} ордеров",
                weak_side,
                target_price,
                sizes.len()
            );

            (true, sizes)
        };

        if should_place {
            super::streams::place_cumulative_second_leg(self, weak_side, target_price, sizes);
        }
    }

    /// Фаза SecondLegPlaced: мониторим условия для переразмещения второй ноги
    ///
    /// Два условия для переразмещения:
    /// 1. weak_bb > placed_price && Trend::Strong && has_good_spread → 0.99 - strong_bb
    /// 2. Trend::Weak && has_good_spread → 0.99 - strong_bb
    fn second_leg_replacement_tick(self: &Arc<Self>, prices: MarketPrices, trend: Trend) {
        let (first_leg_side, placed_price) = {
            let cum_state = self.cumulative_state.lock().unwrap();
            (cum_state.first_leg_side, cum_state.second_leg_placed_price)
        };

        let weak_side = first_leg_side.opposite();
        let strong_bb = prices.bid_for_side(first_leg_side);
        let weak_bb = prices.bid_for_side(weak_side);

        // Определяем цену для переразмещения по условиям
        let target_price = match placed_price {
            Some(placed) => {
                if weak_bb > placed && trend == Trend::Strong {
                    // Условие 1: weak_bb вырос, Strong тренд, хороший спред
                    Self::round_price(0.99 - strong_bb)
                } else if trend == Trend::Weak {
                    // Условие 2: Weak тренд, хороший спред
                    Self::round_price(0.99 - strong_bb)
                } else {
                    // Нет подходящих условий
                    return;
                }
            }
            None => {
                // Все ордера были отменены - размещаем заново при тренде + хорошем спреде
                if trend == Trend::Strong || trend == Trend::Weak {
                    Self::round_price(0.99 - strong_bb)
                } else {
                    return;
                }
            }
        };

        let sizes = {
            let mut cum_state = self.cumulative_state.lock().unwrap();

            if cum_state.pending_second_leg_orders > 0 {
                return;
            }

            // Вторая нога = min(first_leg_filled, base_target_size) - second_leg_filled
            // Используем base_target_size (без компенсации skew) чтобы не накапливать перекос
            let second_leg_target = cum_state.base_target_size.min(cum_state.first_leg_filled);
            let remaining = second_leg_target - cum_state.second_leg_filled;
            let sizes = Self::calculate_order_sizes(remaining, self.config.size);

            if sizes.is_empty() {
                return;
            }

            // Устанавливаем pending для новых ордеров
            cum_state.pending_second_leg_orders = sizes.len() as u32;
            cum_state.second_leg_placed_price = Some(target_price);

            info!(
                "🔄 [Cumulative] Переразмещаем вторую ногу: → {:.2} | {} ордеров | remaining={:.2}",
                target_price,
                sizes.len(),
                remaining
            );

            sizes
        };

        super::streams::place_cumulative_second_leg(self, weak_side, target_price, sizes);
    }

    /// Уменьшает pending_first_leg_orders при отклонении ордера биржей
    /// Проверяет переход в FirstLegPlaced (если есть успешные ордера) или ZeroPoint (если все отклонены)
    pub fn on_cumulative_first_leg_rejected(&self) {
        let mut cum_state = self.cumulative_state.lock().unwrap();

        if cum_state.pending_first_leg_orders > 0 {
            cum_state.pending_first_leg_orders -= 1;
        }

        // Проверяем переход когда все pending обработаны
        if cum_state.phase == CumulativePhase::Beginning && cum_state.pending_first_leg_orders == 0
        {
            if cum_state.first_leg_orders.is_empty() {
                // Все ордера отклонены - возврат в ZeroPoint
                info!("❌ [Cumulative] Beginning → ZeroPoint: все ордера отклонены");
                *cum_state = CumulativeState::default();
            } else {
                // Есть успешные ордера - переход в FirstLegPlaced
                info!(
                    "✅ [Cumulative] Beginning → FirstLegPlaced: {} ордеров подтверждено",
                    cum_state.first_leg_orders.len()
                );
                cum_state.phase = CumulativePhase::FirstLegPlaced;
            }
        }
    }

    /// Уменьшает pending_second_leg_orders при отклонении ордера биржей
    pub fn on_cumulative_second_leg_rejected(&self) {
        let mut cum_state = self.cumulative_state.lock().unwrap();

        if cum_state.pending_second_leg_orders > 0 {
            cum_state.pending_second_leg_orders -= 1;
        }

        // Если все pending обработаны и нет успешных ордеров - возврат в Middle
        if cum_state.phase == CumulativePhase::SecondLegPlaced
            && cum_state.pending_second_leg_orders == 0
            && cum_state.second_leg_orders.is_empty()
        {
            info!("❌ [Cumulative] SecondLegPlaced → Middle: все ордера отклонены");
            cum_state.phase = CumulativePhase::Middle;
            cum_state.second_leg_placed_price = None;
        }
    }

    /// Регистрирует order_id cumulative первой ноги с ценой размещения
    /// Уменьшает pending_first_leg_orders и переходит в FirstLegPlaced когда все ордера подтверждены
    pub fn register_cumulative_first_leg(&self, order_id: String, price: f64) {
        let mut cum_state = self.cumulative_state.lock().unwrap();

        cum_state.first_leg_orders.insert(order_id.clone(), price);

        // Уменьшаем pending counter
        if cum_state.pending_first_leg_orders > 0 {
            cum_state.pending_first_leg_orders -= 1;
        }

        // Переход Beginning → FirstLegPlaced когда все pending ордера подтверждены
        if cum_state.phase == CumulativePhase::Beginning && cum_state.pending_first_leg_orders == 0
        {
            info!(
                "✅ [Cumulative] Beginning → FirstLegPlaced: все {} ордеров подтверждены",
                cum_state.first_leg_orders.len()
            );
            cum_state.phase = CumulativePhase::FirstLegPlaced;
        }
    }

    /// Регистрирует order_id cumulative второй ноги с ценой размещения
    pub fn register_cumulative_second_leg(&self, order_id: String, price: f64) {
        let mut cum_state = self.cumulative_state.lock().unwrap();

        cum_state.second_leg_orders.insert(order_id.clone(), price);

        // Уменьшаем pending counter
        if cum_state.pending_second_leg_orders > 0 {
            cum_state.pending_second_leg_orders -= 1;
        }
    }

    /// Обработка fill cumulative первой ноги
    /// При переходе в Middle отменяет оставшиеся ордера первой ноги
    pub fn on_cumulative_first_leg_fill(
        self: &Arc<Self>,
        order_id: &str,
        fill_size: f64,
        is_fully_filled: bool,
    ) {
        let orders_to_cancel = {
            let mut cum_state = self.cumulative_state.lock().unwrap();

            cum_state.first_leg_filled += fill_size;
            if is_fully_filled {
                cum_state.first_leg_orders.remove(order_id);
            }

            // Проверяем переход FirstLegPlaced → Middle
            let first_remaining = cum_state.first_leg_target_size - cum_state.first_leg_filled;
            if cum_state.phase == CumulativePhase::FirstLegPlaced
                && cum_state.first_leg_filled > 0.0
                && first_remaining < 5.0
            {
                // Собираем ордера для отмены ДО смены состояния (защита от race condition)
                let orders: Vec<String> = cum_state.first_leg_orders.keys().cloned().collect();
                if !orders.is_empty() {
                    info!(
                        "🗑️ [Cumulative] Отменяем {} ордеров первой ноги (переход в Middle)",
                        orders.len()
                    );
                }

                info!(
                    "✅ [Cumulative] FirstLegPlaced → Middle (из fill): {:.2}/{:.2}",
                    cum_state.first_leg_filled, cum_state.first_leg_target_size
                );
                cum_state.phase = CumulativePhase::Middle;
                cum_state.first_leg_placed_price = None;

                orders
            } else {
                Vec::new()
            }
        };

        // Отменяем ордера после освобождения lock
        if !orders_to_cancel.is_empty() {
            super::streams::cancel_orders(self, orders_to_cancel);
        }
    }

    /// Обработка fill cumulative второй ноги
    /// При переходе в ZeroPoint отменяет оставшиеся ордера второй ноги
    pub fn on_cumulative_second_leg_fill(
        self: &Arc<Self>,
        order_id: &str,
        fill_size: f64,
        is_fully_filled: bool,
    ) {
        let orders_to_cancel = {
            let mut cum_state = self.cumulative_state.lock().unwrap();

            cum_state.second_leg_filled += fill_size;
            if is_fully_filled {
                cum_state.second_leg_orders.remove(order_id);
            }

            // Проверяем переход SecondLegPlaced → ZeroPoint
            // Используем base_target_size (без компенсации skew) для проверки завершения
            let second_remaining = cum_state.base_target_size - cum_state.second_leg_filled;
            if cum_state.phase == CumulativePhase::SecondLegPlaced
                && cum_state.second_leg_filled > 0.0
                && second_remaining < 5.0
            {
                // Собираем ордера для отмены ДО сброса состояния (защита от race condition)
                let orders: Vec<String> = cum_state.second_leg_orders.keys().cloned().collect();
                if !orders.is_empty() {
                    info!(
                        "🗑️ [Cumulative] Отменяем {} ордеров второй ноги (переход в ZeroPoint)",
                        orders.len()
                    );
                }

                info!(
                    "🎉 [Cumulative] SecondLegPlaced → ZeroPoint (из fill): Цикл завершен! First: {:.2} | Second: {:.2}",
                    cum_state.first_leg_filled, cum_state.second_leg_filled
                );
                *cum_state = CumulativeState::default();

                orders
            } else {
                Vec::new()
            }
        };

        // Отменяем ордера после освобождения lock
        if !orders_to_cancel.is_empty() {
            super::streams::cancel_orders(self, orders_to_cancel);
        }
    }

    /// Обработка отмены cumulative ордера
    pub fn on_cumulative_order_cancelled(&self, order_id: &str) {
        let mut cum_state = self.cumulative_state.lock().unwrap();

        if cum_state.first_leg_orders.remove(order_id).is_some() {
            // Если все первые ноги отменены - сбрасываем placed_price для переразмещения
            if cum_state.first_leg_orders.is_empty() {
                cum_state.first_leg_placed_price = None;
            }
        } else if cum_state.second_leg_orders.remove(order_id).is_some() {
            // Если все вторые ноги отменены - сбрасываем placed_price для переразмещения
            if cum_state.second_leg_orders.is_empty() {
                cum_state.second_leg_placed_price = None;
            }
        }
    }

    /// Проверяет, является ли order_id cumulative ордером
    /// Возвращает Some(true) = первая нога, Some(false) = вторая нога, None = не cumulative
    pub fn is_cumulative_order(&self, order_id: &str) -> Option<bool> {
        let cum_state = self.cumulative_state.lock().unwrap();

        if cum_state.first_leg_orders.contains_key(order_id) {
            return Some(true);
        }
        if cum_state.second_leg_orders.contains_key(order_id) {
            return Some(false);
        }

        None
    }

    /// Возвращает текущую фазу cumulative стратегии
    #[allow(dead_code)]
    pub fn cumulative_phase(&self) -> CumulativePhase {
        self.cumulative_state.lock().unwrap().phase
    }

    /// Обработка ошибки размещения первой ноги
    /// Сбрасывает состояние в ZeroPoint
    pub fn on_first_leg_placement_failed(&self, reason: &str) {
        let mut cum_state = self.cumulative_state.lock().unwrap();

        info!(
            "❌ [Cumulative] Ошибка размещения первой ноги: {} | Возврат в ZeroPoint",
            reason
        );

        *cum_state = CumulativeState::default();
    }

    /// Обработка ошибки размещения второй ноги
    /// Сбрасывает состояние в Middle для повторной попытки
    pub fn on_second_leg_placement_failed(&self, reason: &str) {
        let mut cum_state = self.cumulative_state.lock().unwrap();

        info!(
            "❌ [Cumulative] Ошибка размещения второй ноги: {} | Возврат в Middle",
            reason
        );

        cum_state.phase = CumulativePhase::Middle;
        cum_state.second_leg_placed_price = None;
        cum_state.pending_second_leg_orders = 0;
        cum_state.second_leg_orders.clear();
    }
}

use super::strat::RealEngine;
use crate::models::{CumulativePhase, CumulativeState, MarketPrices, Trend};
use std::collections::HashSet;
use std::sync::Arc;
use tracing::info;

impl RealEngine {
    /// Основной тик cumulative стратегии
    ///
    /// Фазы:
    /// 1. AccumulatingFirstLeg - набираем позицию на сильной стороне
    /// 2. PlacingSecondLeg - сбрасываем на слабой стороне
    pub fn process_cumulative(self: &Arc<Self>, prices: MarketPrices, trend: Trend) {
        let spread = prices.spread_cents();

        let mut cum_state = self.cumulative_state.lock().unwrap();

        if cum_state.is_none() {
            // Стартуем новый цикл: нужен Trend::Strong + spread 2-3
            if trend != Trend::Strong {
                return;
            }
            if spread < 2 || spread >= 4 {
                return;
            }

            // Проверяем max_balance
            {
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
            }

            let strong_side = prices.strong_side();
            let weak_bb = prices.bid_for_side(prices.weak_side());
            let target_price = Self::round_price(0.99 - weak_bb);

            info!(
                "🚀 [Cumulative] Начинаем новый цикл: {:?} side @ {:.2}",
                strong_side, target_price
            );

            let state = CumulativeState {
                phase: CumulativePhase::AccumulatingFirstLeg,
                first_leg_side: strong_side,
                first_leg_filled: 0.0,
                first_leg_orders: HashSet::new(),
                first_leg_placed_price: None,
                second_leg_filled: 0.0,
                second_leg_orders: HashSet::new(),
                second_leg_placed_price: None,
            };
            *cum_state = Some(state);

            // Размещаем первый батч
            let remaining = self.config.max_size_side;
            let sizes = Self::calculate_order_sizes(remaining, self.config.size);

            cum_state.as_mut().unwrap().first_leg_placed_price = Some(target_price);
            drop(cum_state);

            info!(
                "📦 [Cumulative] Размещаем {} ордеров первой ноги @ {:.2}",
                sizes.len(),
                target_price
            );
            super::streams::place_cumulative_first_leg(self, strong_side, target_price, sizes);

            return;
        }

        // Состояние уже существует
        let state = cum_state.as_mut().unwrap();

        match state.phase {
            CumulativePhase::AccumulatingFirstLeg => {
                // Проверяем, набрали ли мы достаточно (остаток < 5 считаем набранным)
                let first_remaining = self.config.max_size_side - state.first_leg_filled;
                if state.first_leg_filled > 0.0 && first_remaining < 5.0 {
                    info!(
                        "✅ [Cumulative] Первая нога набрана: {:.2}/{:.2} (остаток {:.2} < 5). Переходим к второй ноге.",
                        state.first_leg_filled, self.config.max_size_side, first_remaining
                    );

                    state.phase = CumulativePhase::PlacingSecondLeg;
                    state.first_leg_placed_price = None;

                    return;
                }

                let remaining = Self::round_price(first_remaining);

                // Можем разместить ещё, если есть тренд Strong + spread 2-3 + нет размещений
                if trend == Trend::Strong && spread >= 2 && spread < 4 {
                    let weak_bb = prices.bid_for_side(prices.weak_side());
                    let target_price = Self::round_price(0.99 - weak_bb);

                    let sizes = Self::calculate_order_sizes(remaining, self.config.size);

                    if sizes.is_empty() {
                        return;
                    }

                    state.first_leg_placed_price = Some(target_price);
                    drop(cum_state);

                    info!(
                        "📦 [Cumulative] Доразмещаем {} ордеров первой ноги @ {:.2} (remaining: {:.2})",
                        sizes.len(),
                        target_price,
                        remaining
                    );
                    super::streams::place_cumulative_first_leg(
                        self,
                        prices.strong_side(),
                        target_price,
                        sizes,
                    );
                }
            }
            CumulativePhase::PlacingSecondLeg => {
                // Проверяем, закрыли ли мы полностью (остаток < 5 считаем закрытым)
                let second_remaining = self.config.max_size_side - state.second_leg_filled;
                if state.second_leg_filled > 0.0 && second_remaining < 5.0 {
                    info!(
                        "🎉 [Cumulative] Цикл завершен! First: {:.2} | Second: {:.2} (остаток {:.2} < 5)",
                        state.first_leg_filled, state.second_leg_filled, second_remaining
                    );
                    *cum_state = None;
                    return;
                }

                let weak_side = state.first_leg_side.opposite();
                let remaining = Self::round_price(second_remaining);

                if state.second_leg_placed_price.is_none() {
                    // Начальное размещение второй ноги
                    let weak_bb = prices.bid_for_side(weak_side);
                    let weak_bb_size = prices.bid_size_for_side(weak_side);
                    let strong_bb = prices.bid_for_side(state.first_leg_side);

                    // Выбираем цену: три случая размещения второй ноги
                    let price = if weak_bb_size <= self.config.max_size_side {
                        // Случай 3: Небольшая очередь - присоединяемся к best_bid
                        weak_bb
                    } else if (trend == Trend::Strong || trend == Trend::Weak)
                        && spread >= 2
                        && spread < 4
                    {
                        // Случай 1 и 2: Тренд Strong или Weak + есть спред - выставляем лимитку
                        Self::round_price(0.99 - strong_bb)
                    } else {
                        // Нет подходящих условий
                        return;
                    };

                    let sizes = Self::calculate_order_sizes(remaining, self.config.size);

                    if sizes.is_empty() {
                        return;
                    }

                    state.second_leg_placed_price = Some(price);
                    drop(cum_state);

                    info!(
                        "📦 [Cumulative] Размещаем {} ордеров второй ноги {:?} @ {:.2} (remaining: {:.2})",
                        sizes.len(),
                        weak_side,
                        price,
                        remaining
                    );
                    super::streams::place_cumulative_second_leg(self, weak_side, price, sizes);
                } else if (trend == Trend::Strong || trend == Trend::Weak)
                    && spread >= 2
                    && spread < 4
                {
                    // Переразмещение второй ноги по лучшей цене (при тренде Strong или Weak)
                    let strong_bb = prices.bid_for_side(state.first_leg_side);
                    let new_price = Self::round_price(0.99 - strong_bb);

                    let current_price = state.second_leg_placed_price.unwrap();
                    if (new_price - current_price).abs() == 0.0 {
                        return; // Цена не изменилась
                    }

                    info!(
                        "🔄 [Cumulative] Переразмещаем вторые ноги: {:.2} → {:.2}",
                        current_price, new_price
                    );

                    state.second_leg_placed_price = None;

                    let sizes = Self::calculate_order_sizes(remaining, self.config.size);
                    drop(cum_state);

                    if sizes.is_empty() {
                        return;
                    }

                    // Устанавливаем новую цену
                    {
                        let mut cum_state = self.cumulative_state.lock().unwrap();
                        if let Some(state) = cum_state.as_mut() {
                            state.second_leg_placed_price = Some(new_price);
                        }
                    }

                    info!(
                        "📦 [Cumulative] Переразмещаем {} ордеров второй ноги {:?} @ {:.2}",
                        sizes.len(),
                        weak_side,
                        new_price
                    );
                    super::streams::place_cumulative_second_leg(self, weak_side, new_price, sizes);
                }
            }
        }
    }

    /// Регистрирует order_id cumulative первой ноги
    pub fn register_cumulative_first_leg(&self, order_id: String) {
        let mut cum_state = self.cumulative_state.lock().unwrap();
        if let Some(state) = cum_state.as_mut() {
            state.first_leg_orders.insert(order_id.clone());
            info!("📝 [Cumulative] Первая нога зарегистрирована: {}", order_id);
        }
    }

    /// Регистрирует order_id cumulative второй ноги
    pub fn register_cumulative_second_leg(&self, order_id: String) {
        let mut cum_state = self.cumulative_state.lock().unwrap();
        if let Some(state) = cum_state.as_mut() {
            state.second_leg_orders.insert(order_id.clone());
            info!("📝 [Cumulative] Вторая нога зарегистрирована: {}", order_id);
        }
    }

    /// Обработка fill cumulative первой ноги
    pub fn on_cumulative_first_leg_fill(
        &self,
        order_id: &str,
        fill_size: f64,
        is_fully_filled: bool,
    ) {
        let mut cum_state = self.cumulative_state.lock().unwrap();
        if let Some(state) = cum_state.as_mut() {
            state.first_leg_filled += fill_size;
            if is_fully_filled {
                state.first_leg_orders.remove(order_id);
            }
            info!(
                "📊 [Cumulative] Первая нога fill: +{:.2} = {:.2}/{:.2} (orders: {})",
                fill_size,
                state.first_leg_filled,
                self.config.max_size_side,
                state.first_leg_orders.len()
            );
        }
    }

    /// Обработка fill cumulative второй ноги
    pub fn on_cumulative_second_leg_fill(
        &self,
        order_id: &str,
        fill_size: f64,
        is_fully_filled: bool,
    ) {
        let mut cum_state = self.cumulative_state.lock().unwrap();
        if let Some(state) = cum_state.as_mut() {
            state.second_leg_filled += fill_size;
            if is_fully_filled {
                state.second_leg_orders.remove(order_id);
            }
            info!(
                "📊 [Cumulative] Вторая нога fill: +{:.2} = {:.2}/{:.2} (orders: {})",
                fill_size,
                state.second_leg_filled,
                state.first_leg_filled,
                state.second_leg_orders.len()
            );
        }
    }

    /// Обработка отмены cumulative ордера
    pub fn on_cumulative_order_cancelled(&self, order_id: &str) {
        let mut cum_state = self.cumulative_state.lock().unwrap();
        if let Some(state) = cum_state.as_mut() {
            if state.first_leg_orders.remove(order_id) {
                if state.first_leg_orders.is_empty() {
                    state.first_leg_placed_price = None;
                }
                info!(
                    "🗑️ [Cumulative] Первая нога отменена: {} (remaining orders: {})",
                    order_id,
                    state.first_leg_orders.len()
                );
            } else if state.second_leg_orders.remove(order_id) {
                if state.second_leg_orders.is_empty() {
                    state.second_leg_placed_price = None;
                }
                info!(
                    "🗑️ [Cumulative] Вторая нога отменена: {} (remaining orders: {})",
                    order_id,
                    state.second_leg_orders.len()
                );
            }
        }
    }

    /// Проверяет, является ли order_id cumulative ордером
    /// Возвращает Some(true) = первая нога, Some(false) = вторая нога, None = не cumulative
    pub fn is_cumulative_order(&self, order_id: &str) -> Option<bool> {
        let cum_state = self.cumulative_state.lock().unwrap();
        if let Some(state) = cum_state.as_ref() {
            if state.first_leg_orders.contains(order_id) {
                return Some(true);
            }
            if state.second_leg_orders.contains(order_id) {
                return Some(false);
            }
        }
        None
    }
}

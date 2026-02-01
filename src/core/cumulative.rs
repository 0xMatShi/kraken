use super::strat::RealEngine;
use crate::models::{CumulativePhase, CumulativeState, MarketPrices, Trend};
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
        let has_good_spread = spread >= 2 && spread < 4;

        match phase {
            CumulativePhase::ZeroPoint => {
                // Начинаем цикл только при тренде Strong + спред 2-3
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
                if trend == Trend::Strong {
                    self.first_leg_replacement_tick(prices);
                }
            }
            CumulativePhase::Middle => {
                // Размещаем вторую ногу при любом тренде (Strong, Weak или None)
                self.middle_tick(prices, trend);
            }
            CumulativePhase::SecondLegPlaced => {
                // Переразмещаем вторую ногу при тренде Weak
                if trend == Trend::Weak {
                    self.second_leg_replacement_tick(prices);
                }
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
    fn start_tick(self: &Arc<Self>, prices: MarketPrices) {
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
            let target_size = self.config.max_size_side;

            let sizes = Self::calculate_order_sizes(target_size, self.config.size);
            if sizes.is_empty() {
                return;
            }

            // Переходим в Beginning и устанавливаем pending_first_leg_orders
            cum_state.phase = CumulativePhase::Beginning;
            cum_state.first_leg_side = strong_side;
            cum_state.first_leg_target_size = target_size;
            cum_state.first_leg_placed_price = Some(target_price);
            cum_state.pending_first_leg_orders = sizes.len() as u32;

            info!(
                "🚀 [Cumulative] ZeroPoint → Beginning: {:?} @ {:.2} | {} ордеров | target_size={:.2}",
                strong_side,
                target_price,
                sizes.len(),
                target_size
            );

            (strong_side, target_price, sizes)
        };

        // Размещаем первую ногу (state уже в Beginning)
        super::streams::place_cumulative_first_leg(self, strong_side, target_price, sizes);
    }

    /// Фаза FirstLegPlaced: мониторим тренд strong для переразмещения первой ноги
    ///
    /// Условия (проверяются в process_cumulative):
    /// - Trend::Strong
    /// - Spread 2-3 цента
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
    /// 1. Тренд Strong + спред 2-3 → размещаем по 0.99 - strong_bb
    /// 2. Тренд Weak + спред 2-3 → размещаем по 0.99 - strong_bb
    /// 3. На best_bid слабой стороны акций < max_size_side → присоединяемся к best_bid
    fn middle_tick(self: &Arc<Self>, prices: MarketPrices, trend: Trend) {
        let spread = prices.spread_cents();

        let (weak_side, strong_side, target_size) = {
            let cum_state = self.cumulative_state.lock().unwrap();
            (
                cum_state.first_leg_side.opposite(),
                cum_state.first_leg_side,
                cum_state.first_leg_target_size,
            )
        };

        let weak_bb = prices.bid_for_side(weak_side);
        let weak_bb_size = prices.bid_size_for_side(weak_side);
        let strong_bb = prices.bid_for_side(strong_side);

        // Определяем цену для второй ноги
        let target_price = if weak_bb_size <= self.config.max_size_side {
            // Случай 3: небольшая очередь - присоединяемся к best_bid
            weak_bb
        } else if (trend == Trend::Strong || trend == Trend::Weak) && spread >= 2 && spread < 4 {
            // Случай 1 и 2: тренд + спред - размещаем лимитку
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

            let remaining = target_size - cum_state.second_leg_filled;
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

    /// Фаза SecondLegPlaced: мониторим тренд для переразмещения второй ноги
    ///
    /// Условия (проверяются в process_cumulative):
    /// - Trend::Strong или Trend::Weak
    /// - Spread 2-3 цента
    ///
    /// При выполнении условий:
    /// - Переразмещаем по новой цене (0.99 - strong_bb)
    fn second_leg_replacement_tick(self: &Arc<Self>, prices: MarketPrices) {
        let (first_leg_side, target_size) = {
            let cum_state = self.cumulative_state.lock().unwrap();
            (cum_state.first_leg_side, cum_state.first_leg_target_size)
        };

        let weak_side = first_leg_side.opposite();
        let strong_bb = prices.bid_for_side(first_leg_side);
        let target_price = Self::round_price(0.99 - strong_bb);

        let sizes = {
            let mut cum_state = self.cumulative_state.lock().unwrap();

            if cum_state.pending_second_leg_orders > 0 {
                return;
            }

            let remaining = target_size - cum_state.second_leg_filled;
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

    /// Регистрирует order_id cumulative первой ноги
    /// Уменьшает pending_first_leg_orders и переходит в FirstLegPlaced когда все ордера подтверждены
    pub fn register_cumulative_first_leg(&self, order_id: String) {
        let mut cum_state = self.cumulative_state.lock().unwrap();

        cum_state.first_leg_orders.insert(order_id.clone());

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

    /// Регистрирует order_id cumulative второй ноги
    pub fn register_cumulative_second_leg(&self, order_id: String) {
        let mut cum_state = self.cumulative_state.lock().unwrap();

        cum_state.second_leg_orders.insert(order_id.clone());

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
                let orders: Vec<String> = cum_state.first_leg_orders.iter().cloned().collect();
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
            let second_remaining = cum_state.first_leg_target_size - cum_state.second_leg_filled;
            if cum_state.phase == CumulativePhase::SecondLegPlaced
                && cum_state.second_leg_filled > 0.0
                && second_remaining < 5.0
            {
                // Собираем ордера для отмены ДО сброса состояния (защита от race condition)
                let orders: Vec<String> = cum_state.second_leg_orders.iter().cloned().collect();
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

        if cum_state.first_leg_orders.remove(order_id) {
            // Если все первые ноги отменены - сбрасываем placed_price для переразмещения
            if cum_state.first_leg_orders.is_empty() {
                cum_state.first_leg_placed_price = None;
            }
        } else if cum_state.second_leg_orders.remove(order_id) {
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

        if cum_state.first_leg_orders.contains(order_id) {
            return Some(true);
        }
        if cum_state.second_leg_orders.contains(order_id) {
            return Some(false);
        }

        None
    }

    /// Возвращает текущую фазу cumulative стратегии
    #[allow(dead_code)]
    pub fn cumulative_phase(&self) -> CumulativePhase {
        self.cumulative_state.lock().unwrap().phase
    }
}

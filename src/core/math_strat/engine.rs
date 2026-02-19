use super::super::strat::RealEngine;
use super::decision::compute_decision;
use super::metrics::ObiMetrics;
use super::state::MathOrder;
use crate::models::{MarketPrices, Side};
use crate::ui::{self, ORDER_BOOK_DEPTH, OrderLevel};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::info;

/// Максимальное время жизни Math ордера
const MAX_ORDER_LIFETIME: Duration = Duration::from_secs(10);

/// Порог разворота EMA OBI(1) для отмены ордеров
const EMA_REVERSAL_THRESHOLD: f64 = 0.4;

impl RealEngine {
    /// Обновляет OBI метрики в UI (вызывается для ВСЕХ стратегий)
    pub fn update_obi_display(&self) {
        let (up_bids, _) = ui::get_up_book(&self.ui_state);
        let (down_bids, _) = ui::get_down_book(&self.ui_state);

        // Вычисляем Δt для Time-Weighted EMA
        let delta_ms = {
            let mut last = self.last_obi_update.lock().unwrap();
            let now = Instant::now();
            let dt = match *last {
                Some(prev) => prev.elapsed().as_secs_f64() * 1000.0,
                // Первый вызов: используем τ как Δt → α ≈ 0.632 (холодный старт)
                None => super::metrics::EMA_TAU_MS,
            };
            *last = Some(now);
            dt
        };

        let (prev_ema_v, prev_ema_sh) = {
            let state = self.ui_state.lock().unwrap();
            (state.obi_display.ema_obi1_v, state.obi_display.ema_obi1_sh)
        };

        let metrics =
            super::metrics::compute_metrics(&up_bids, &down_bids, delta_ms, prev_ema_v, prev_ema_sh);

        let mut state = self.ui_state.lock().unwrap();
        let d = &mut state.obi_display;
        d.slice_v = metrics.slice_v;
        d.slice_sh = metrics.slice_sh;
        d.obi1_v = metrics.obi1_v;
        d.obi1_sh = metrics.obi1_sh;
        d.ema_obi1_v = metrics.ema_obi1_v;
        d.ema_obi1_sh = metrics.ema_obi1_sh;
        d.wobi_v = metrics.wobi_v;
        d.wobi_sh = metrics.wobi_sh;
        d.consensus_v = metrics.consensus_v;
        d.consensus_sh = metrics.consensus_sh;
        d.gradient_v = metrics.gradient_v;
        d.gradient_sh = metrics.gradient_sh;
    }

    /// Основная логика Math стратегии — вызывается из process_tick()
    pub fn process_math(self: &Arc<Self>, _prices: MarketPrices) {
        // Читаем текущие OBI метрики из ui_state (уже обновлены в update_obi_display)
        let metrics = {
            let state = self.ui_state.lock().unwrap();
            let d = &state.obi_display;
            ObiMetrics {
                slice_v: d.slice_v,
                slice_sh: d.slice_sh,
                obi1_v: d.obi1_v,
                obi1_sh: d.obi1_sh,
                ema_obi1_v: d.ema_obi1_v,
                ema_obi1_sh: d.ema_obi1_sh,
                wobi_v: d.wobi_v,
                wobi_sh: d.wobi_sh,
                consensus_v: d.consensus_v,
                consensus_sh: d.consensus_sh,
                gradient_v: d.gradient_v,
                gradient_sh: d.gradient_sh,
            }
        };

        // Обновляем счётчики подтверждения градиента
        let gradient_confirm = {
            let mut s = self.math_state.lock().unwrap();
            if metrics.gradient_v > 0.15 {
                s.gradient_confirm_pos += 1;
                s.gradient_confirm_neg = 0;
            } else if metrics.gradient_v < -0.15 {
                s.gradient_confirm_neg += 1;
                s.gradient_confirm_pos = 0;
            } else {
                s.gradient_confirm_pos = 0;
                s.gradient_confirm_neg = 0;
            }
            s.gradient_confirm_pos >= 2 || s.gradient_confirm_neg >= 2
        };

        // Отменяем устаревшие ордера
        self.cancel_stale_math_orders(&metrics);

        // Вычисляем решение о размещении
        let decision = compute_decision(&metrics, gradient_confirm);

        // Нет уровней для размещения — выходим
        if decision.up_levels.is_empty() && decision.down_levels.is_empty() {
            return;
        }

        let order_size = self.config.size;
        let max_per_side = (self.config.max_size_side / order_size).floor() as usize;
        if max_per_side == 0 {
            return;
        }

        let (up_bids, _) = ui::get_up_book(&self.ui_state);
        let (down_bids, _) = ui::get_down_book(&self.ui_state);

        // UP ордера
        if !decision.up_levels.is_empty() {
            // Эффективная позиция = исполненные + активные (committed) ордера
            // UP не должен опережать DOWN по эффективной позиции
            let (active, pending, balance_ok) = {
                let s = self.math_state.lock().unwrap();
                let eff_up = s.up_filled
                    + (s.up_orders.len() + s.pending_up_levels.len()) as f64 * order_size;
                let eff_down = s.down_filled
                    + (s.down_orders.len() + s.pending_down_levels.len()) as f64 * order_size;
                (s.up_orders.len(), s.pending_up_levels.len(), eff_up <= eff_down)
            };

            if active + pending < max_per_side && balance_ok {
                for &level in &decision.up_levels {
                    // Проверяем баланс внутри lock — уже с учётом pending из предыдущих итераций
                    let can_place = {
                        let mut s = self.math_state.lock().unwrap();
                        let occupied = s.up_orders.values().any(|o| o.level == level)
                            || s.pending_up_levels.contains(&level);
                        let slots_ok = s.up_orders.len() + s.pending_up_levels.len() < max_per_side;
                        let eff_up = s.up_filled
                            + (s.up_orders.len() + s.pending_up_levels.len()) as f64 * order_size;
                        let eff_down = s.down_filled
                            + (s.down_orders.len() + s.pending_down_levels.len()) as f64
                                * order_size;
                        let balance_ok = eff_up <= eff_down;
                        if !occupied && slots_ok && balance_ok {
                            s.pending_up_levels.insert(level);
                            true
                        } else {
                            false
                        }
                    };
                    if !can_place {
                        continue;
                    }
                    if let Some(price) = Self::bid_price_at_level(&up_bids, level) {
                        super::streams::place_math_order(self, Side::Up, price, order_size, level);
                    } else {
                        // Нет цены — снимаем резерв
                        self.math_state
                            .lock()
                            .unwrap()
                            .pending_up_levels
                            .remove(&level);
                    }
                }
            }
        }

        // DOWN ордера
        if !decision.down_levels.is_empty() {
            // Эффективная позиция = исполненные + активные (committed) ордера
            // DOWN не должен опережать UP по эффективной позиции
            let (active, pending, balance_ok) = {
                let s = self.math_state.lock().unwrap();
                let eff_up = s.up_filled
                    + (s.up_orders.len() + s.pending_up_levels.len()) as f64 * order_size;
                let eff_down = s.down_filled
                    + (s.down_orders.len() + s.pending_down_levels.len()) as f64 * order_size;
                (s.down_orders.len(), s.pending_down_levels.len(), eff_down <= eff_up)
            };

            if active + pending < max_per_side && balance_ok {
                for &level in &decision.down_levels {
                    let can_place = {
                        let mut s = self.math_state.lock().unwrap();
                        let occupied = s.down_orders.values().any(|o| o.level == level)
                            || s.pending_down_levels.contains(&level);
                        let slots_ok =
                            s.down_orders.len() + s.pending_down_levels.len() < max_per_side;
                        let eff_up = s.up_filled
                            + (s.up_orders.len() + s.pending_up_levels.len()) as f64 * order_size;
                        let eff_down = s.down_filled
                            + (s.down_orders.len() + s.pending_down_levels.len()) as f64
                                * order_size;
                        let balance_ok = eff_down <= eff_up;
                        if !occupied && slots_ok && balance_ok {
                            s.pending_down_levels.insert(level);
                            true
                        } else {
                            false
                        }
                    };
                    if !can_place {
                        continue;
                    }
                    if let Some(price) = Self::bid_price_at_level(&down_bids, level) {
                        super::streams::place_math_order(
                            self,
                            Side::Down,
                            price,
                            order_size,
                            level,
                        );
                    } else {
                        self.math_state
                            .lock()
                            .unwrap()
                            .pending_down_levels
                            .remove(&level);
                    }
                }
            }
        }
    }

    /// Возвращает цену bid по уровню (1-based) из стакана
    fn bid_price_at_level(bids: &[OrderLevel; ORDER_BOOK_DEPTH], level: u8) -> Option<f64> {
        if level == 0 {
            return None;
        }
        let idx = (level as usize) - 1;
        if idx < bids.len() && bids[idx].size > 0.0 {
            Some(bids[idx].price)
        } else {
            None
        }
    }

    /// Отменяет устаревшие Math ордера (>10с или EMA OBI(1) развернулся более чем на 0.4)
    fn cancel_stale_math_orders(self: &Arc<Self>, metrics: &ObiMetrics) {
        let ema = metrics.ema_obi1_v;

        let stale_ids: Vec<String> = {
            let s = self.math_state.lock().unwrap();
            s.up_orders
                .iter()
                .filter(|(_, o)| {
                    o.placed_at.elapsed() > MAX_ORDER_LIFETIME || ema < -EMA_REVERSAL_THRESHOLD
                })
                .map(|(id, _)| id.clone())
                .chain(
                    s.down_orders
                        .iter()
                        .filter(|(_, o)| {
                            o.placed_at.elapsed() > MAX_ORDER_LIFETIME
                                || ema > EMA_REVERSAL_THRESHOLD
                        })
                        .map(|(id, _)| id.clone()),
                )
                .collect()
        };

        if !stale_ids.is_empty() {
            {
                let mut s = self.math_state.lock().unwrap();
                for id in &stale_ids {
                    s.up_orders.remove(id);
                    s.down_orders.remove(id);
                }
            }
            info!("🗑️ [Math] Отменяем {} устаревших ордеров", stale_ids.len());
            super::super::streams::cancel_orders(self, stale_ids);
        }

    }

    /// Регистрирует Math ордер после получения order_id от биржи
    pub fn register_math_order(
        &self,
        order_id: String,
        side: Side,
        price: f64,
        _size: f64,
        level: u8,
    ) {
        let order = MathOrder {
            level,
            placed_at: Instant::now(),
        };
        let mut s = self.math_state.lock().unwrap();
        match side {
            Side::Up => {
                s.pending_up_levels.remove(&level);
                s.up_orders.insert(order_id.clone(), order);
            }
            Side::Down => {
                s.pending_down_levels.remove(&level);
                s.down_orders.insert(order_id.clone(), order);
            }
        }
        info!(
            "📝 [Math] Ордер зарегистрирован: {} {:?} @ {:.2} уровень {}",
            order_id, side, price, level
        );
    }

    /// Обрабатывает исполнение Math ордера (полное или частичное)
    pub fn on_math_order_fill(&self, order_id: &str, fill_size: f64, is_fully_filled: bool) {
        let mut s = self.math_state.lock().unwrap();
        if s.up_orders.contains_key(order_id) {
            info!(
                "✅ [Math] UP ордер исполнен: {} size={:.2} full={}",
                order_id, fill_size, is_fully_filled
            );
            if is_fully_filled {
                s.up_orders.remove(order_id);
                s.up_filled += fill_size;
            }
        } else if s.down_orders.contains_key(order_id) {
            info!(
                "✅ [Math] DOWN ордер исполнен: {} size={:.2} full={}",
                order_id, fill_size, is_fully_filled
            );
            if is_fully_filled {
                s.down_orders.remove(order_id);
                s.down_filled += fill_size;
            }
        }
    }

    /// Проверяет принадлежность ордера к Math стратегии
    /// Возвращает Some(true) = UP, Some(false) = DOWN, None = не Math ордер
    pub fn is_math_order(&self, order_id: &str) -> Option<bool> {
        let s = self.math_state.lock().unwrap();
        if s.up_orders.contains_key(order_id) {
            Some(true)
        } else if s.down_orders.contains_key(order_id) {
            Some(false)
        } else {
            None
        }
    }

    /// Снимает резерв pending-уровня при ошибке размещения
    pub fn cancel_pending_level(&self, side: Side, level: u8) {
        let mut s = self.math_state.lock().unwrap();
        match side {
            Side::Up => {
                s.pending_up_levels.remove(&level);
            }
            Side::Down => {
                s.pending_down_levels.remove(&level);
            }
        }
    }

    /// Обрабатывает отмену Math ордера
    pub fn on_math_order_cancelled(&self, order_id: &str) {
        let mut s = self.math_state.lock().unwrap();
        if s.up_orders.remove(order_id).is_some() {
            info!("🗑️ [Math] UP ордер отменён: {}", order_id);
        } else if s.down_orders.remove(order_id).is_some() {
            info!("🗑️ [Math] DOWN ордер отменён: {}", order_id);
        }
    }
}

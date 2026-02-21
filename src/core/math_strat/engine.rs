use super::super::strat::RealEngine;
use super::decision::compute_decision;
use super::metrics::ObiMetrics;
use super::state::MathOrder;
use crate::models::{MarketPrices, Side};
use crate::ui::{self, ORDER_BOOK_DEPTH, OrderLevel};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::info;

/// Порог разворота EMA OBI(1) для немедленной отмены ордеров
const EMA_REVERSAL_THRESHOLD: f64 = 0.4;

/// Минимальный интервал между циклами управления позицией
const POSITION_CHECK_INTERVAL: Duration = Duration::from_millis(500);

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

        let (prev_ema_v, prev_ema_sh, prev_ema_wobi_v, prev_ema_wobi_sh) = {
            let state = self.ui_state.lock().unwrap();
            (
                state.obi_display.ema_obi1_v,
                state.obi_display.ema_obi1_sh,
                state.obi_display.ema_wobi_v,
                state.obi_display.ema_wobi_sh,
            )
        };

        let metrics = super::metrics::compute_metrics(
            &up_bids,
            &down_bids,
            delta_ms,
            prev_ema_v,
            prev_ema_sh,
            prev_ema_wobi_v,
            prev_ema_wobi_sh,
        );

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
        d.ema_wobi_v = metrics.ema_wobi_v;
        d.ema_wobi_sh = metrics.ema_wobi_sh;
        d.consensus_v = metrics.consensus_v;
        d.consensus_sh = metrics.consensus_sh;
        d.gradient_v = metrics.gradient_v;
        d.gradient_sh = metrics.gradient_sh;
    }

    /// Основная логика Math стратегии — вызывается из process_tick()
    pub fn process_math(self: &Arc<Self>, _prices: MarketPrices) {
        // Throttle: цикл управления позицией не чаще раза в 500мс
        {
            let mut s = self.math_state.lock().unwrap();
            let elapsed = s.last_position_check.map_or(Duration::MAX, |t| t.elapsed());
            if elapsed < POSITION_CHECK_INTERVAL {
                return;
            }
            s.last_position_check = Some(Instant::now());
        }

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
                ema_wobi_v: d.ema_wobi_v,
                ema_wobi_sh: d.ema_wobi_sh,
                consensus_v: d.consensus_v,
                consensus_sh: d.consensus_sh,
                gradient_v: d.gradient_v,
                gradient_sh: d.gradient_sh,
            }
        };

        // Обновляем счётчики подтверждения градиента
        let gradient_confirm = {
            let mut s = self.math_state.lock().unwrap();
            if metrics.gradient_sh > 0.15 {
                s.gradient_confirm_pos += 1;
                s.gradient_confirm_neg = 0;
            } else if metrics.gradient_sh < -0.15 {
                s.gradient_confirm_neg += 1;
                s.gradient_confirm_pos = 0;
            } else {
                s.gradient_confirm_pos = 0;
                s.gradient_confirm_neg = 0;
            }
            s.gradient_confirm_pos >= 2 || s.gradient_confirm_neg >= 2
        };

        // Правило EMA-разворота: немедленно отменяем ордера при резком развороте
        self.check_ema_reversal(&metrics);

        // Вычисляем решение о размещении
        let decision = compute_decision(&metrics, gradient_confirm);

        // Гистерезис по реальному перекосу портфеля:
        // пауза стороны: когда её перевес >= max_size_side
        // снятие паузы:  когда перевес <= max_size_side - size/2
        let resume_threshold = self.config.max_size_side - self.config.size / 2.0;
        let (up_paused, down_paused) = {
            let p = self.portfolio.lock().unwrap();
            let mut s = self.math_state.lock().unwrap();
            let up_skew = p.up_shares - p.down_shares;
            let down_skew = p.down_shares - p.up_shares;
            if up_skew >= self.config.max_size_side {
                s.up_paused = true;
            } else if up_skew <= resume_threshold {
                s.up_paused = false;
            }
            if down_skew >= self.config.max_size_side {
                s.down_paused = true;
            } else if down_skew <= resume_threshold {
                s.down_paused = false;
            }
            (s.up_paused, s.down_paused)
        };

        let order_size = self.config.size;
        let max_per_side = (self.config.max_size_side / order_size).floor() as usize;
        if max_per_side == 0 {
            return;
        }

        let (up_bids, _) = ui::get_up_book(&self.ui_state);
        let (down_bids, _) = ui::get_down_book(&self.ui_state);

        // Сверяем активные ордера с новым решением и при необходимости переставляем
        if !up_paused {
            self.reconcile_math_side(
                Side::Up,
                &decision.up_levels,
                &up_bids,
                order_size,
                max_per_side,
            );
        }
        if !down_paused {
            self.reconcile_math_side(
                Side::Down,
                &decision.down_levels,
                &down_bids,
                order_size,
                max_per_side,
            );
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

    /// Правило EMA-разворота: если EMA OBI(1) sh резко развернулся — отменяем ордера на стороне
    fn check_ema_reversal(self: &Arc<Self>, metrics: &ObiMetrics) {
        let ema = metrics.ema_obi1_sh;

        // EMA резко медвежий → отменяем все UP ордера
        if ema < -EMA_REVERSAL_THRESHOLD {
            let ids: Vec<String> = {
                let mut s = self.math_state.lock().unwrap();
                let ids: Vec<_> = s.up_orders.keys().cloned().collect();
                for id in &ids {
                    s.up_orders.remove(id);
                }
                ids
            };
            if !ids.is_empty() {
                info!(
                    "🚨 [Math] EMA разворот вниз ({:.2}): отменяем {} UP ордеров",
                    ema,
                    ids.len()
                );
                super::super::streams::cancel_orders(self, ids);
            }
        }

        // EMA резко бычий → отменяем все DOWN ордера
        if ema > EMA_REVERSAL_THRESHOLD {
            let ids: Vec<String> = {
                let mut s = self.math_state.lock().unwrap();
                let ids: Vec<_> = s.down_orders.keys().cloned().collect();
                for id in &ids {
                    s.down_orders.remove(id);
                }
                ids
            };
            if !ids.is_empty() {
                info!(
                    "🚨 [Math] EMA разворот вверх ({:.2}): отменяем {} DOWN ордеров",
                    ema,
                    ids.len()
                );
                super::super::streams::cancel_orders(self, ids);
            }
        }
    }

    /// Сверяет активные ордера с новым решением:
    /// — отменяет ордера, где уровень вышел из решения или цена в стакане изменилась
    /// — размещает новые ордера для уровней без активного ордера
    fn reconcile_math_side(
        self: &Arc<Self>,
        side: Side,
        new_levels: &[u8],
        bids: &[OrderLevel; ORDER_BOOK_DEPTH],
        order_size: f64,
        max_per_side: usize,
    ) {
        // Желаемое состояние: уровень → текущая цена из стакана
        let desired: HashMap<u8, f64> = new_levels
            .iter()
            .filter_map(|&lvl| Self::bid_price_at_level(bids, lvl).map(|p| (lvl, p)))
            .collect();

        // Определяем ордера для отмены:
        // — уровень не входит в новое решение
        // — ИЛИ цена в стакане изменилась с момента размещения (>= 0.5 цента)
        let to_cancel: Vec<String> = {
            let s = self.math_state.lock().unwrap();
            let orders = if matches!(side, Side::Up) {
                &s.up_orders
            } else {
                &s.down_orders
            };
            orders
                .iter()
                .filter(|(_, order)| match desired.get(&order.level) {
                    Some(&desired_price) => (desired_price - order.price).abs() >= 0.005,
                    None => true,
                })
                .map(|(id, _)| id.clone())
                .collect()
        };

        if !to_cancel.is_empty() {
            info!(
                "[Math] {:?}: переставляем {} ордеров (уровни/цены изменились)",
                side,
                to_cancel.len()
            );
            {
                let mut s = self.math_state.lock().unwrap();
                let orders = if matches!(side, Side::Up) {
                    &mut s.up_orders
                } else {
                    &mut s.down_orders
                };
                for id in &to_cancel {
                    orders.remove(id);
                }
            }
            super::super::streams::cancel_orders(self, to_cancel);
        }

        // Размещаем ордера для желаемых уровней без активного или pending ордера
        for (&level, &price) in &desired {
            let can_place = {
                let mut s = self.math_state.lock().unwrap();
                let (level_active, level_pending, active_len, pending_len) =
                    if matches!(side, Side::Up) {
                        (
                            s.up_orders.values().any(|o| o.level == level),
                            s.pending_up_levels.contains(&level),
                            s.up_orders.len(),
                            s.pending_up_levels.len(),
                        )
                    } else {
                        (
                            s.down_orders.values().any(|o| o.level == level),
                            s.pending_down_levels.contains(&level),
                            s.down_orders.len(),
                            s.pending_down_levels.len(),
                        )
                    };

                if !level_active && !level_pending && active_len + pending_len < max_per_side {
                    if matches!(side, Side::Up) {
                        s.pending_up_levels.insert(level);
                    } else {
                        s.pending_down_levels.insert(level);
                    }
                    true
                } else {
                    false
                }
            };

            if can_place {
                super::streams::place_math_order(self, side, price, order_size, level);
            }
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
        let order = MathOrder { level, price };
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

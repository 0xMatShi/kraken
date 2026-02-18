use std::collections::HashMap;
use std::time::Instant;

/// Один активный math ордер
#[derive(Debug)]
pub struct MathOrder {
    pub level: u8, // уровень размещения (1-7)
    pub placed_at: Instant,
}

/// Состояние math стратегии
#[derive(Debug)]
pub struct MathState {
    /// Активные UP ордера: order_id → MathOrder
    pub up_orders: HashMap<String, MathOrder>,
    /// Активные DOWN ордера: order_id → MathOrder
    pub down_orders: HashMap<String, MathOrder>,
    /// Суммарно исполнено на UP стороне
    pub up_filled: f64,
    /// Суммарно исполнено на DOWN стороне
    pub down_filled: f64,
    /// Счётчик подтверждения положительного градиента (>= 2 тиков = сигнал подтверждён)
    pub gradient_confirm_pos: u32,
    /// Счётчик подтверждения отрицательного градиента
    pub gradient_confirm_neg: u32,
}

impl Default for MathState {
    fn default() -> Self {
        Self {
            up_orders: HashMap::new(),
            down_orders: HashMap::new(),
            up_filled: 0.0,
            down_filled: 0.0,
            gradient_confirm_pos: 0,
            gradient_confirm_neg: 0,
        }
    }
}

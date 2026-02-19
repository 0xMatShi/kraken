use crate::ui::{ORDER_BOOK_DEPTH, OrderLevel};

/// Срезы стакана: (start_idx, end_idx, метка)
/// Slice 0 = уровень 1 (best bid)
/// Slice 1 = уровни 2-3
/// Slice 2 = уровни 4-5
/// Slice 3 = уровни 6-7
const SLICES: [(usize, usize); 4] = [(0, 1), (1, 3), (3, 5), (5, 7)];

/// Представительные глубины для весов WOBI (срезы 1-3)
const WOBI_DEPTHS: [f64; 3] = [2.0, 4.0, 6.0];

/// Параметр затухания весов WOBI
pub const LAMBDA: f64 = 0.15;

/// Постоянная времени Time-Weighted EMA (τ в миллисекундах)
/// α_t = 1 - exp(-Δt / τ), где Δt — реальное время между тиками
pub const EMA_TAU_MS: f64 = 500.0;

/// Вычисленные OBI метрики
#[derive(Debug, Clone, Default)]
pub struct ObiMetrics {
    /// V_OBI и Sh_OBI по 4 срезам [1, 2-3, 4-5, 6-7]
    pub slice_v: [f64; 4],
    pub slice_sh: [f64; 4],

    /// OBI(1) raw (slice 0)
    pub obi1_v: f64,
    pub obi1_sh: f64,

    /// OBI(1) EMA сглаженный (alpha=0.3)
    pub ema_obi1_v: f64,
    pub ema_obi1_sh: f64,

    /// WOBI (взвешенный по срезам 1-3, lambda=0.15)
    pub wobi_v: f64,
    pub wobi_sh: f64,

    /// Consensus (среднее sgn по всем 4 срезам)
    pub consensus_v: f64,
    pub consensus_sh: f64,

    /// Gradient = OBI_near (срез 1) - OBI_far (срез 3)
    pub gradient_v: f64,
    pub gradient_sh: f64,
}

/// Вычисляет V_OBI и Sh_OBI для заданного среза уровней стакана
fn slice_obi(
    up_bids: &[OrderLevel; ORDER_BOOK_DEPTH],
    down_bids: &[OrderLevel; ORDER_BOOK_DEPTH],
    start: usize,
    end: usize,
) -> (f64, f64) {
    let count = end.saturating_sub(start);

    let up_sh: f64 = up_bids
        .iter()
        .skip(start)
        .take(count)
        .filter(|l| l.size > 0.0)
        .map(|l| l.size)
        .sum();
    let up_v: f64 = up_bids
        .iter()
        .skip(start)
        .take(count)
        .filter(|l| l.size > 0.0)
        .map(|l| l.price * l.size)
        .sum();

    let dn_sh: f64 = down_bids
        .iter()
        .skip(start)
        .take(count)
        .filter(|l| l.size > 0.0)
        .map(|l| l.size)
        .sum();
    let dn_v: f64 = down_bids
        .iter()
        .skip(start)
        .take(count)
        .filter(|l| l.size > 0.0)
        .map(|l| l.price * l.size)
        .sum();

    let sh_total = up_sh + dn_sh;
    let sh_obi = if sh_total > 0.0 {
        (up_sh - dn_sh) / sh_total
    } else {
        0.0
    };

    let v_total = up_v + dn_v;
    let v_obi = if v_total > 0.0 {
        (up_v - dn_v) / v_total
    } else {
        0.0
    };

    (v_obi, sh_obi)
}

/// Вычисляет все OBI метрики по текущему состоянию стакана
///
/// `delta_ms` — реальное время в мс с предыдущего вызова (для Time-Weighted EMA)
/// `prev_ema_v/sh` — предыдущие значения EMA для сглаживания OBI(1)
pub fn compute_metrics(
    up_bids: &[OrderLevel; ORDER_BOOK_DEPTH],
    down_bids: &[OrderLevel; ORDER_BOOK_DEPTH],
    delta_ms: f64,
    prev_ema_v: f64,
    prev_ema_sh: f64,
) -> ObiMetrics {
    // Вычисляем OBI по каждому срезу
    let mut slice_v = [0.0f64; 4];
    let mut slice_sh = [0.0f64; 4];

    for (i, &(start, end)) in SLICES.iter().enumerate() {
        let (v, sh) = slice_obi(up_bids, down_bids, start, end);
        slice_v[i] = v;
        slice_sh[i] = sh;
    }

    // OBI(1) = slice 0 (best bid)
    let obi1_v = slice_v[0];
    let obi1_sh = slice_sh[0];

    // Time-Weighted EMA: α_t = 1 - exp(-Δt / τ)
    // Первый тик (delta=0 или очень маленький) → alpha близок к 0, сохраняем предыдущее
    // Большой Δt (долгая пауза) → alpha близок к 1, берём свежее значение
    let alpha_t = 1.0 - (-delta_ms / EMA_TAU_MS).exp();
    let ema_obi1_v = alpha_t * obi1_v + (1.0 - alpha_t) * prev_ema_v;
    let ema_obi1_sh = alpha_t * obi1_sh + (1.0 - alpha_t) * prev_ema_sh;

    // WOBI: взвешенная сумма срезов 1-3 (исключая slice 0 = уровень 1)
    // Веса: экспоненциальное затухание exp(-lambda*d), нормализованные
    let raw_w: Vec<f64> = WOBI_DEPTHS.iter().map(|&d| (-LAMBDA * d).exp()).collect();
    let sum_w: f64 = raw_w.iter().sum();
    let weights: Vec<f64> = raw_w.iter().map(|w| w / sum_w).collect();

    let wobi_v: f64 = weights.iter().zip(&slice_v[1..]).map(|(w, &v)| w * v).sum();
    let wobi_sh: f64 = weights
        .iter()
        .zip(&slice_sh[1..])
        .map(|(w, &sh)| w * sh)
        .sum();

    // Consensus: (1/N) * sum(sgn(OBI)) по всем 4 срезам
    let sgn = |x: f64| -> f64 {
        if x > 0.001 {
            1.0
        } else if x < -0.001 {
            -1.0
        } else {
            0.0
        }
    };
    let n = 4.0f64;
    let consensus_v: f64 = slice_v.iter().map(|&v| sgn(v)).sum::<f64>() / n;
    let consensus_sh: f64 = slice_sh.iter().map(|&sh| sgn(sh)).sum::<f64>() / n;

    // Gradient: near (срез 1 = уровни 2-3) - far (срез 3 = уровни 6-7)
    let gradient_v = slice_v[1] - slice_v[3];
    let gradient_sh = slice_sh[1] - slice_sh[3];

    ObiMetrics {
        slice_v,
        slice_sh,
        obi1_v,
        obi1_sh,
        ema_obi1_v,
        ema_obi1_sh,
        wobi_v,
        wobi_sh,
        consensus_v,
        consensus_sh,
        gradient_v,
        gradient_sh,
    }
}

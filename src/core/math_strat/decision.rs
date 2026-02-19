use super::metrics::ObiMetrics;

/// Решение о размещении ордеров
#[derive(Debug, Clone, Default)]
pub struct PlacementDecision {
    /// Уровни для размещения UP ордеров (1-7, пустой = не ставить)
    pub up_levels: Vec<u8>,
    /// Уровни для размещения DOWN ордеров (1-7, пустой = не ставить)
    pub down_levels: Vec<u8>,
}

/// Определяет уровень размещения на основе градиента
/// Gradient > 0.15 → уровень 1 (импульс у рынка)
/// Gradient < -0.15 → уровень 3 (накопление в глубине)
/// Neutral → уровень 2
fn level_from_gradient(gradient: f64) -> u8 {
    if gradient > 0.15 {
        1
    } else if gradient < -0.15 {
        3
    } else {
        2
    }
}

/// Полная матрица решений (60+ комбинаций, шаг 0.1)
///
/// Приоритеты принятия решения (от сильного к слабому):
/// 1. Зона молчания (все метрики слабые) → не торговать
/// 2. Расхождение V_OBI и Sh_OBI (Category 6) → доверяем V
/// 3. Шоковый импульс (все экстремальные) → уровень 1 немедленно
/// 4. Полный консенсус (Consensus ±0.75+, WOBI ±0.5+) → уровень 1-2
/// 5. Расхождение фронта и тыла (OBI1 vs WOBI противоположны)
/// 6. Импульс (OBI1 + WOBI + Consensus + Gradient)
/// 7. Накопление (OBI1 нейтральный + WOBI сильный + Gradient) - требует подтверждения
/// 8. Только OBI(1) силён (5.2) → контр-лимитка
/// 9. WOBI силён, Consensus слаб (5.3) → глубокий вход
/// 10. Нейтральные метрики (5.1) → маркет-мейкер обе стороны 3-4
/// 11. Общий случай со слабыми сигналами
pub fn compute_decision(metrics: &ObiMetrics, gradient_confirm: bool) -> PlacementDecision {
    let obi1 = metrics.ema_obi1_v; // EMA сглаженный OBI(1)
    let wobi = metrics.wobi_v; // V-weighted WOBI (первичный, шаги 2-10)
    let wobi_sh = metrics.wobi_sh; // Sh-weighted WOBI (для расхождения в шаге 1)
    let consensus = metrics.consensus_v;
    let gradient = metrics.gradient_v;

    // ─── Шаг 0: Зона молчания ───────────────────────────────────────────────
    // Все метрики слабее порогов → не торговать
    if consensus.abs() <= 0.15 && wobi.abs() <= 0.2 && gradient.abs() <= 0.1 && obi1.abs() <= 0.2 {
        return PlacementDecision::default();
    }

    // ─── Шаг 1: Расхождение V_OBI и Sh_OBI (Category 6) ───────────────────
    // wobi = V-weighted (крупные деньги), wobi_sh = Sh-weighted (количество ордеров)
    let v_and_sh_both_significant = wobi.abs() > 0.2 && wobi_sh.abs() > 0.2;
    let v_sh_opposite = v_and_sh_both_significant
        && ((wobi > 0.0 && wobi_sh < 0.0) || (wobi < 0.0 && wobi_sh > 0.0));
    let v_sh_diverge_strong = v_sh_opposite && (wobi - wobi_sh).abs() > 0.3;

    if v_sh_diverge_strong {
        if wobi > 0.2 && wobi_sh < -0.2 {
            // 6.2: V бычий, Sh медвежий → доверяем V → UP 2-3
            return PlacementDecision {
                up_levels: vec![2, 3],
                down_levels: vec![],
            };
        }
        if wobi < -0.2 && wobi_sh > 0.2 {
            // 6.1: V медвежий, Sh бычий → доверяем V → DOWN 2-3
            return PlacementDecision {
                up_levels: vec![],
                down_levels: vec![2, 3],
            };
        }
    }

    // ─── Шаг 2: Шоковый импульс ─────────────────────────────────────────────
    // Gradient > ±0.4, OBI(1) > ±0.4, Consensus ±0.8 → немедленный вход уровень 1
    if obi1.abs() > 0.4 && wobi.abs() > 0.4 && gradient.abs() > 0.4 && consensus.abs() >= 0.75 {
        return if wobi > 0.0 {
            PlacementDecision {
                up_levels: vec![1],
                down_levels: vec![],
            }
        } else {
            PlacementDecision {
                up_levels: vec![],
                down_levels: vec![1],
            }
        };
    }

    // ─── Шаг 3: Полный консенсус (1.1 / 1.2) ────────────────────────────────
    // OBI1 > ±0.3, WOBI > ±0.5, Consensus ±0.75 → агрессивный вход
    if obi1.abs() > 0.3 && wobi.abs() > 0.5 && consensus.abs() >= 0.75 {
        return if wobi > 0.0 {
            // 1.1: Полный бычий консенсус
            if gradient > 0.1 {
                // Импульс у рынка → UP 1-2, DOWN страховка 6-7
                PlacementDecision {
                    up_levels: vec![1, 2],
                    down_levels: vec![6, 7],
                }
            } else if gradient > -0.1 {
                PlacementDecision {
                    up_levels: vec![2, 3],
                    down_levels: vec![6, 7],
                }
            } else {
                PlacementDecision {
                    up_levels: vec![3, 4],
                    down_levels: vec![6, 7],
                }
            }
        } else {
            // 1.2: Полный медвежий консенсус
            if gradient < -0.1 {
                PlacementDecision {
                    up_levels: vec![6, 7],
                    down_levels: vec![1, 2],
                }
            } else if gradient < 0.1 {
                PlacementDecision {
                    up_levels: vec![6, 7],
                    down_levels: vec![2, 3],
                }
            } else {
                PlacementDecision {
                    up_levels: vec![6, 7],
                    down_levels: vec![3, 4],
                }
            }
        };
    }

    // Подвид полного консенсуса: WOBI ±0.3-0.5, Consensus ±0.75 (немного слабее)
    if obi1.abs() > 0.3 && wobi.abs() > 0.3 && consensus.abs() >= 0.75 {
        let base_level = level_from_gradient(if wobi > 0.0 { gradient } else { -gradient });
        return if wobi > 0.0 {
            PlacementDecision {
                up_levels: vec![base_level, (base_level + 1).min(5)],
                down_levels: vec![],
            }
        } else {
            PlacementDecision {
                up_levels: vec![],
                down_levels: vec![base_level, (base_level + 1).min(5)],
            }
        };
    }

    // ─── Шаг 4: Расхождение фронта и тыла (4.1 / 4.2) ──────────────────────
    // OBI(1) и WOBI имеют противоположные знаки
    let obi1_wobi_opposite = obi1.abs() > 0.3
        && wobi.abs() > 0.3
        && ((obi1 > 0.0 && wobi < 0.0) || (obi1 < 0.0 && wobi > 0.0));

    if obi1_wobi_opposite {
        if obi1 < -0.3 && wobi > 0.3 && consensus >= 0.4 {
            // 4.1: Медвежий фронт / бычий тыл → покупка на откате UP 2-3
            let lvl = if gradient < -0.1 { 3u8 } else { 2u8 };
            return PlacementDecision {
                up_levels: vec![lvl, (lvl + 1).min(5)],
                down_levels: vec![],
            };
        }
        if obi1 > 0.3 && wobi < -0.3 && consensus <= -0.4 {
            // 4.2: Бычий фронт / медвежий тыл → ловушка DOWN 3-4
            let lvl = if gradient > 0.1 { 3u8 } else { 3u8 };
            return PlacementDecision {
                up_levels: vec![],
                down_levels: vec![lvl, (lvl + 1).min(5)],
            };
        }
        // Менее чёткое расхождение: слабый консенсус
        if obi1 < -0.3 && wobi > 0.2 {
            return PlacementDecision {
                up_levels: vec![3, 4],
                down_levels: vec![],
            };
        }
        if obi1 > 0.3 && wobi < -0.2 {
            return PlacementDecision {
                up_levels: vec![],
                down_levels: vec![3, 4],
            };
        }
        // Значительное расхождение без чёткого сигнала → не торговать
        return PlacementDecision::default();
    }

    // ─── Шаг 5: Импульс (2.1 / 2.2) ─────────────────────────────────────────
    // OBI1 > ±0.3, WOBI > ±0.3, Consensus ±0.5
    if obi1.abs() > 0.3 && wobi.abs() > 0.3 && consensus.abs() >= 0.5 {
        return if wobi > 0.0 {
            // Бычий импульс
            if gradient > 0.15 {
                // 2.1: Явный импульс → уровень 1
                PlacementDecision {
                    up_levels: vec![1],
                    down_levels: vec![],
                }
            } else if gradient > -0.1 {
                PlacementDecision {
                    up_levels: vec![2],
                    down_levels: vec![],
                }
            } else {
                PlacementDecision {
                    up_levels: vec![2, 3],
                    down_levels: vec![],
                }
            }
        } else {
            // Медвежий импульс
            if gradient < -0.15 {
                // 2.2: Явный импульс → уровень 1
                PlacementDecision {
                    up_levels: vec![],
                    down_levels: vec![1],
                }
            } else if gradient < 0.1 {
                PlacementDecision {
                    up_levels: vec![],
                    down_levels: vec![2],
                }
            } else {
                PlacementDecision {
                    up_levels: vec![],
                    down_levels: vec![2, 3],
                }
            }
        };
    }

    // Более слабый импульс: OBI1 > ±0.2, WOBI > ±0.2, Consensus ±0.5
    if obi1.abs() > 0.2 && wobi.abs() > 0.2 && consensus.abs() >= 0.5 {
        let base_level = level_from_gradient(if wobi > 0.0 { gradient } else { -gradient });
        let adj = (base_level + 1).min(5);
        return if wobi > 0.0 {
            PlacementDecision {
                up_levels: vec![adj],
                down_levels: vec![],
            }
        } else {
            PlacementDecision {
                up_levels: vec![],
                down_levels: vec![adj],
            }
        };
    }

    // ─── Шаг 6: Накопление (3.1 / 3.2) ─────────────────────────────────────
    // OBI1 нейтральный, WOBI > ±0.3, Consensus ±0.5
    if obi1.abs() <= 0.25 && wobi.abs() > 0.3 && consensus.abs() >= 0.5 {
        return if wobi > 0.0 {
            if gradient < -0.15 {
                // 3.1: Бычье накопление → UP 3-4 (требует подтверждения)
                if gradient_confirm {
                    PlacementDecision {
                        up_levels: vec![3, 4],
                        down_levels: vec![],
                    }
                } else {
                    PlacementDecision {
                        up_levels: vec![4],
                        down_levels: vec![],
                    }
                }
            } else if gradient > 0.1 {
                // Накопление + импульс
                PlacementDecision {
                    up_levels: vec![2, 3],
                    down_levels: vec![],
                }
            } else {
                // Нейтральный градиент
                PlacementDecision {
                    up_levels: vec![3],
                    down_levels: vec![],
                }
            }
        } else {
            if gradient > 0.15 {
                // 3.2: Медвежье накопление → DOWN 3-4 (требует подтверждения)
                if gradient_confirm {
                    PlacementDecision {
                        up_levels: vec![],
                        down_levels: vec![3, 4],
                    }
                } else {
                    PlacementDecision {
                        up_levels: vec![],
                        down_levels: vec![4],
                    }
                }
            } else if gradient < -0.1 {
                PlacementDecision {
                    up_levels: vec![],
                    down_levels: vec![2, 3],
                }
            } else {
                PlacementDecision {
                    up_levels: vec![],
                    down_levels: vec![3],
                }
            }
        };
    }

    // Слабое накопление: WOBI > ±0.2, Consensus ±0.5
    if obi1.abs() <= 0.25 && wobi.abs() > 0.2 && consensus.abs() >= 0.5 {
        let lvl = if gradient.abs() > 0.1 { 3u8 } else { 4u8 };
        return if wobi > 0.0 {
            PlacementDecision {
                up_levels: vec![lvl],
                down_levels: vec![],
            }
        } else {
            PlacementDecision {
                up_levels: vec![],
                down_levels: vec![lvl],
            }
        };
    }

    // ─── Шаг 7: 5.2 — только OBI(1) силён ──────────────────────────────────
    if obi1.abs() > 0.5 && wobi.abs() <= 0.2 && consensus.abs() <= 0.3 {
        // Временный ордер у лучшего бида → ожидаем коррекцию → контр-лимитка
        return if obi1 > 0.5 {
            // UP OBI(1) силён → противоположный DOWN 2-3
            PlacementDecision {
                up_levels: vec![],
                down_levels: vec![2, 3],
            }
        } else {
            PlacementDecision {
                up_levels: vec![2, 3],
                down_levels: vec![],
            }
        };
    }

    // Более мягкий вариант 5.2: OBI(1) > ±0.3, WOBI слабый
    if obi1.abs() > 0.3 && wobi.abs() <= 0.25 && consensus.abs() <= 0.3 {
        return if obi1 > 0.3 {
            PlacementDecision {
                up_levels: vec![],
                down_levels: vec![3, 4],
            }
        } else {
            PlacementDecision {
                up_levels: vec![3, 4],
                down_levels: vec![],
            }
        };
    }

    // ─── Шаг 8: 5.3 — WOBI силён, Consensus слаб ────────────────────────────
    if wobi.abs() > 0.5 && consensus.abs() <= 0.3 {
        return if wobi > 0.0 {
            PlacementDecision {
                up_levels: vec![4, 5],
                down_levels: vec![],
            }
        } else {
            PlacementDecision {
                up_levels: vec![],
                down_levels: vec![4, 5],
            }
        };
    }

    if wobi.abs() > 0.35 && consensus.abs() <= 0.3 {
        return if wobi > 0.0 {
            PlacementDecision {
                up_levels: vec![5],
                down_levels: vec![],
            }
        } else {
            PlacementDecision {
                up_levels: vec![],
                down_levels: vec![5],
            }
        };
    }

    // ─── Шаг 9: 5.1 — Нейтральные метрики → маркет-мейкер ──────────────────
    if consensus.abs() <= 0.25 && wobi.abs() <= 0.25 && obi1.abs() <= 0.3 {
        return PlacementDecision {
            up_levels: vec![3, 4],
            down_levels: vec![3, 4],
        };
    }

    // ─── Шаг 10: Общий случай — слабые сигналы ──────────────────────────────
    if wobi.abs() > 0.2 {
        let base_level = level_from_gradient(if wobi > 0.0 { gradient } else { -gradient });
        // Слабый консенсус → на уровень глубже
        let adj_level = if consensus.abs() >= 0.5 {
            base_level
        } else {
            (base_level + 1).min(5)
        };
        return if wobi > 0.0 {
            PlacementDecision {
                up_levels: vec![adj_level],
                down_levels: vec![],
            }
        } else {
            PlacementDecision {
                up_levels: vec![],
                down_levels: vec![adj_level],
            }
        };
    }

    // Consensus без WOBI — слабый сигнал
    if consensus.abs() > 0.5 {
        return if consensus > 0.0 {
            PlacementDecision {
                up_levels: vec![4, 5],
                down_levels: vec![],
            }
        } else {
            PlacementDecision {
                up_levels: vec![],
                down_levels: vec![4, 5],
            }
        };
    }

    PlacementDecision::default()
}

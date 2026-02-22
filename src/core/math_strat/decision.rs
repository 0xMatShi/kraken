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
/// 2. Шоковый импульс (все экстремальные) → уровень 1 немедленно
///    2а. Шоковый + противоположная структура → поздний вход 4-5
/// 3. Полный консенсус (Consensus ±0.75+, WOBI ±0.5+) → уровень 1-2
///    с детализацией DOWN-страховки по силе сигнала и gradient
/// 4. Расхождение фронта и тыла (OBI1 vs WOBI противоположны)
///    с уточнением для бычьего накопления при gradient < -0.2
/// 5. Импульс: умеренный (OBI 0.5+, WOBI 0.4+) и слабый (OBI 0.3-0.4)
///    с разными уровнями по gradient для каждого подтипа
/// 6. Накопление (OBI1 нейтральный + WOBI сильный + Gradient) - требует подтверждения
/// 7. Только OBI(1) силён (5.2) → контр-лимитка, с уточнением по gradient
/// 8. WOBI силён, Consensus слаб (5.3) → глубокий вход
/// 9. Нейтральные метрики (5.1) → маркет-мейкер, с gradient → однонаправленно
/// 10. Пограничный бычий/медвежий (OBI 0.2-0.3, WOBI 0.1-0.2) → уровень 4
/// 11. Общий случай со слабыми сигналами
pub fn compute_decision(metrics: &ObiMetrics, gradient_confirm: bool) -> PlacementDecision {
    let obi1 = metrics.ema_obi1_sh; // EMA сглаженный OBI(1)
    let consensus = metrics.consensus_sh;
    let gradient = metrics.gradient_sh;

    // ─── Шаг 0: Зона молчания ───────────────────────────────────────────────
    // Все метрики слабее порогов → встать 2-3 в обе стороны
    let wobi = metrics.ema_wobi_sh;
    if consensus.abs() <= 0.25 && wobi.abs() <= 0.2 && gradient.abs() <= 0.1 && obi1.abs() <= 0.2
    {
        return PlacementDecision {
                up_levels: vec![2, 3],
                down_levels: vec![2, 3],
            }
    }

    // ─── Шаг 2: Шоковый импульс ─────────────────────────────────────────────
    // Gradient > ±0.4, OBI(1) > ±0.4, Consensus ±0.75 → немедленный вход уровень 1
    if obi1.abs() > 0.4 && wobi.abs() > 0.4 && gradient.abs() > 0.4 && consensus.abs() >= 0.75 {
        return if wobi > 0.0 {
            PlacementDecision {
                up_levels: vec![2],
                down_levels: vec![4],
            }
        } else {
            PlacementDecision {
                up_levels: vec![4],
                down_levels: vec![2],
            }
        };
    }

    // Шаг 2а: Шоковый импульс + противоположная структура → поздний вход 4-5 (строки 46-47)
    // OBI бычий но WOBI медвежий + Consensus медвежий + Gradient-шок → ловушка, DOWN поздно
    if obi1 > 0.3 && wobi < -0.3 && consensus <= -0.5 && gradient > 0.5 {
        return PlacementDecision {
            up_levels: vec![2, 3],
            down_levels: vec![4, 5],
        };
    }
    // OBI медвежий но WOBI бычий + Consensus бычий + Gradient-шок → откат, UP поздно
    if obi1 < -0.3 && wobi > 0.3 && consensus >= 0.5 && gradient < -0.5 {
        return PlacementDecision {
            up_levels: vec![4, 5],
            down_levels: vec![2, 3],
        };
    }

    // ─── Шаг 3: Полный консенсус (1.1 / 1.2) ────────────────────────────────
    // OBI1 > ±0.3, WOBI > ±0.5, Consensus ±0.75 → агрессивный вход
    if obi1.abs() > 0.3 && wobi.abs() > 0.5 && consensus.abs() >= 0.75 {
        return if wobi > 0.0 {
            // 1.1: Полный бычий консенсус
            // Максимальный (OBI 0.7+, WOBI 0.7+): DOWN страховка зависит от gradient
            if obi1 > 0.7 && wobi > 0.7 {
                if gradient > 0.2 {
                    // Импульс + максимальный → UP 1, DOWN 5 (строка: OBI 0.9+, grad > +0.2)
                    PlacementDecision {
                        up_levels: vec![2],
                        down_levels: vec![5],
                    }
                } else if gradient > -0.2 {
                    // Нейтраль + максимальный → UP 1, DOWN 5 (строка: OBI 0.9+, grad ≈ 0)
                    PlacementDecision {
                        up_levels: vec![2],
                        down_levels: vec![5],
                    }
                } else {
                    // Накопление + максимальный → UP 1, DOWN 6 (строка: OBI 0.9+, grad < -0.2)
                    PlacementDecision {
                        up_levels: vec![2],
                        down_levels: vec![6],
                    }
                }
            } else if gradient > 0.1 {
                // Импульс у рынка → UP 1-2, DOWN страховка 6-7
                PlacementDecision {
                    up_levels: vec![2, 3],
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
            if obi1 < -0.7 && wobi < -0.7 {
                if gradient < -0.2 {
                    // DOWN 1, UP нет
                    PlacementDecision {
                        up_levels: vec![5],
                        down_levels: vec![2],
                    }
                } else if gradient < 0.2 {
                    // DOWN 1, UP 7
                    PlacementDecision {
                        up_levels: vec![5],
                        down_levels: vec![2],
                    }
                } else {
                    // DOWN 1, UP 6
                    PlacementDecision {
                        up_levels: vec![6],
                        down_levels: vec![2],
                    }
                }
            } else if gradient < -0.1 {
                PlacementDecision {
                    up_levels: vec![6, 7],
                    down_levels: vec![2, 3],
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

    // Подвид полного консенсуса: WOBI ±0.3-0.5, Consensus ±0.5 (немного слабее)
    if obi1.abs() > 0.3 && wobi.abs() > 0.3 && consensus.abs() >= 0.5 {
        let base_level = level_from_gradient(if wobi > 0.0 { gradient } else { -gradient });
        return if wobi > 0.0 {
            PlacementDecision {
                up_levels: vec![base_level, (base_level + 1).min(5)],
                down_levels: vec![(base_level + 1).min(5), (base_level + 2).min(5)],
            }
        } else {
            PlacementDecision {
                up_levels: vec![(base_level + 1).min(5), (base_level + 2).min(5)],
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
            // 4.1: Медвежий фронт / бычий тыл → покупка на откате
            // При gradient < -0.2 (бычье накопление) → UP 2 точнее (строка 30)
            if gradient < -0.2 {
                return PlacementDecision {
                    up_levels: vec![2],
                    down_levels: vec![3],
                };
            }
            let lvl = if gradient < -0.1 { 3u8 } else { 2u8 };
            return PlacementDecision {
                up_levels: vec![lvl, (lvl + 1).min(5)],
                down_levels: vec![],
            };
        }
        if obi1 > 0.3 && wobi < -0.3 && consensus <= -0.4 {
            // 4.2: Бычий фронт / медвежий тыл → ловушка DOWN 3-4
            return PlacementDecision {
                up_levels: vec![2],
                down_levels: vec![3, 4],
            };
        }
        // Менее чёткое расхождение: слабый консенсус
        if obi1 < -0.3 && wobi > 0.2 {
            return PlacementDecision {
                up_levels: vec![3, 4],
                down_levels: vec![2],
            };
        }
        if obi1 > 0.3 && wobi < -0.2 {
            return PlacementDecision {
                up_levels: vec![2],
                down_levels: vec![3, 4],
            };
        }
        // Значительное расхождение без чёткого сигнала → не торговать
        return PlacementDecision::default();
    }

    // ─── Шаг 5: Импульс — умеренный (OBI 0.5+, WOBI 0.4+, Consensus ±0.5) ──
    // Строки документа: OBI 0.5-0.6, WOBI 0.4-0.5
    // gradient > +0.2 → UP 1-2; neutral → UP 2; gradient < -0.2 → UP 3
    if obi1.abs() > 0.4 && wobi.abs() > 0.35 && consensus.abs() >= 0.5 {
        return if wobi > 0.0 {
            if gradient > 0.2 {
                PlacementDecision {
                    up_levels: vec![2, 3],
                    down_levels: vec![3, 4],
                }
            } else if gradient > -0.1 {
                PlacementDecision {
                    up_levels: vec![2],
                    down_levels: vec![3],
                }
            } else {
                PlacementDecision {
                    up_levels: vec![3],
                    down_levels: vec![2],
                }
            }
        } else {
            if gradient < -0.2 {
                PlacementDecision {
                    up_levels: vec![3, 4],
                    down_levels: vec![2, 3],
                }
            } else if gradient < 0.1 {
                PlacementDecision {
                    up_levels: vec![3],
                    down_levels: vec![2],
                }
            } else {
                PlacementDecision {
                    up_levels: vec![2],
                    down_levels: vec![3],
                }
            }
        };
    }

    // ─── Шаг 5б: Импульс — слабый (OBI 0.3-0.4, WOBI 0.3-0.4, Consensus ±0.5) ─
    // Строки документа: OBI 0.3-0.4, WOBI 0.3-0.4
    // gradient > +0.2 → UP 2; neutral → UP 2-3; gradient < -0.2 → UP 3-4
    if obi1.abs() > 0.3 && wobi.abs() > 0.3 && consensus.abs() >= 0.5 {
        return if wobi > 0.0 {
            if gradient > 0.2 {
                PlacementDecision {
                    up_levels: vec![2],
                    down_levels: vec![3],
                }
            } else if gradient > -0.1 {
                PlacementDecision {
                    up_levels: vec![2, 3],
                    down_levels: vec![3, 4],
                }
            } else {
                PlacementDecision {
                    up_levels: vec![3, 4],
                    down_levels: vec![2, 3],
                }
            }
        } else {
            if gradient < -0.2 {
                PlacementDecision {
                    up_levels: vec![3],
                    down_levels: vec![2],
                }
            } else if gradient < 0.1 {
                PlacementDecision {
                    up_levels: vec![3, 4],
                    down_levels: vec![2, 3],
                }
            } else {
                PlacementDecision {
                    up_levels: vec![2, 3],
                    down_levels: vec![3, 4],
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
                down_levels: vec![adj + 1],
            }
        } else {
            PlacementDecision {
                up_levels: vec![adj + 1],
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
                        down_levels: vec![2, 3],
                    }
                } else {
                    PlacementDecision {
                        up_levels: vec![4],
                        down_levels: vec![2],
                    }
                }
            } else if gradient > 0.1 {
                // Накопление + импульс
                PlacementDecision {
                    up_levels: vec![2, 3],
                    down_levels: vec![4, 5],
                }
            } else {
                // Нейтральный градиент
                PlacementDecision {
                    up_levels: vec![2],
                    down_levels: vec![3],
                }
            }
        } else {
            if gradient > 0.15 {
                // 3.2: Медвежье накопление → DOWN 3-4 (требует подтверждения)
                if gradient_confirm {
                    PlacementDecision {
                        up_levels: vec![2, 3],
                        down_levels: vec![3, 4],
                    }
                } else {
                    PlacementDecision {
                        up_levels: vec![2],
                        down_levels: vec![4],
                    }
                }
            } else if gradient < -0.1 {
                PlacementDecision {
                    up_levels: vec![4, 5],
                    down_levels: vec![2, 3],
                }
            } else {
                PlacementDecision {
                    up_levels: vec![2],
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
                down_levels: vec![lvl + 1],
            }
        } else {
            PlacementDecision {
                up_levels: vec![lvl + 1],
                down_levels: vec![lvl],
            }
        };
    }

    // ─── Шаг 7: 5.2 — только OBI(1) силён ──────────────────────────────────
    // Временный ордер у лучшего бида → ожидаем коррекцию → контр-лимитка
    if obi1.abs() > 0.5 && wobi.abs() <= 0.2 && consensus.abs() <= 0.5 {
        return if obi1 > 0.5 {
            // OBI UP силён + gradient-импульс → DOWN 2 (точнее, строка 26)
            // OBI UP силён без импульса → DOWN 2-3 (строка 25)
            if gradient > 0.2 {
                PlacementDecision {
                    up_levels: vec![2],
                    down_levels: vec![2],
                }
            } else {
                PlacementDecision {
                    up_levels: vec![2],
                    down_levels: vec![2, 3],
                }
            }
        } else {
            // Симметрично для OBI DOWN
            if gradient < -0.2 {
                PlacementDecision {
                    up_levels: vec![2],
                    down_levels: vec![2],
                }
            } else {
                PlacementDecision {
                    up_levels: vec![2, 3],
                    down_levels: vec![2],
                }
            }
        };
    }

    // Более мягкий вариант 5.2: OBI(1) > ±0.3, WOBI слабый
    if obi1.abs() > 0.3 && wobi.abs() <= 0.25 && consensus.abs() <= 0.5 {
        return if obi1 > 0.3 {
            PlacementDecision {
                up_levels: vec![2],
                down_levels: vec![3, 4],
            }
        } else {
            PlacementDecision {
                up_levels: vec![3, 4],
                down_levels: vec![2],
            }
        };
    }

    // ─── Шаг 8: 5.3 — WOBI силён, Consensus слаб ────────────────────────────
    if wobi.abs() > 0.5 && consensus.abs() <= 0.5 {
        return if wobi > 0.0 {
            PlacementDecision {
                up_levels: vec![3, 4],
                down_levels: vec![3],
            }
        } else {
            PlacementDecision {
                up_levels: vec![3],
                down_levels: vec![3, 4],
            }
        };
    }

    if wobi.abs() > 0.35 && consensus.abs() <= 0.5 {
        return if wobi > 0.0 {
            PlacementDecision {
                up_levels: vec![3],
                down_levels: vec![4],
            }
        } else {
            PlacementDecision {
                up_levels: vec![4],
                down_levels: vec![3],
            }
        };
    }

    // ─── Шаг 9: 5.1 — Нейтральные метрики → маркет-мейкер ──────────────────
    // При наличии gradient → однонаправленная лимитка уровень 3 (строки 32-33)
    // Без gradient → обе стороны 3-4 (маркет-мейкер)
    if consensus.abs() <= 0.25 && wobi.abs() <= 0.25 && obi1.abs() <= 0.3 {
        if gradient > 0.2 {
            // Нейтрал + слабый импульс UP → UP 3
            return PlacementDecision {
                up_levels: vec![3],
                down_levels: vec![4],
            };
        } else if gradient < -0.2 {
            // Нейтрал + слабый импульс DOWN → DOWN 3
            return PlacementDecision {
                up_levels: vec![4],
                down_levels: vec![3],
            };
        }
        return PlacementDecision {
            up_levels: vec![3, 4],
            down_levels: vec![3, 4],
        };
    }

    // ─── Шаг 10: Пограничный бычий/медвежий (строки 50-51) ─────────────────
    // OBI 0.2-0.3, WOBI 0.1-0.2, Consensus ±0.5 → глубокая лимитка уровень 4
    if obi1.abs() > 0.2
        && obi1.abs() <= 0.3
        && wobi.abs() > 0.1
        && wobi.abs() <= 0.2
        && consensus.abs() >= 0.5
    {
        return if obi1 > 0.0 {
            PlacementDecision {
                up_levels: vec![3],
                down_levels: vec![4],
            }
        } else {
            PlacementDecision {
                up_levels: vec![4],
                down_levels: vec![3],
            }
        };
    }

    // ─── Шаг 11: Общий случай — слабые сигналы ──────────────────────────────
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
                down_levels: vec![adj_level + 1],
            }
        } else {
            PlacementDecision {
                up_levels: vec![adj_level + 1],
                down_levels: vec![adj_level],
            }
        };
    }

    PlacementDecision::default()
}

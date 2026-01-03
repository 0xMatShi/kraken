use std::sync::Mutex;
use std::time::{Instant, Duration};
use std::collections::VecDeque;
use chrono::Utc;
use crate::models::{Portfolio, Side, TradeRecord, MarketPrices};
use crate::report::Reporter;

const VIRTUAL_BALANCE: f64 = 3000.0; // Максимальный бюджет на одно событие
const SIZE: f64 = 15.0;              // Размер обычной лимитки (shares)
const HEDGE_SIZE: f64 = 30.0;        // Порог перекоса для хеджирования

pub struct DryRunEngine {
    portfolio: Mutex<Portfolio>,
    reporter: Reporter,
    active_limit_orders: Mutex<Vec<VirtualOrder>>,
    // Очередь для симуляции пинга
    tick_buffer: Mutex<VecDeque<(Instant, MarketPrices)>>,
    latency: Duration,
}

struct VirtualOrder {
    side: Side,
    price: f64,
    shares: f64,
    created_at: Instant,
}

impl DryRunEngine {
    pub fn new(slug: &str, latency_ms: u64) -> Self {
        Self {
            portfolio: Mutex::new(Portfolio::default()),
            reporter: Reporter::new(slug),
            active_limit_orders: Mutex::new(Vec::new()),
            tick_buffer: Mutex::new(VecDeque::new()),
            latency: Duration::from_millis(latency_ms),
        }
    }

    pub fn process_tick(&self, prices: MarketPrices) {
        // 1. Симуляция задержки: кладем тик в буфер
        let mut buffer = self.tick_buffer.lock().unwrap();
        buffer.push_back((Instant::now(), prices));

        // 2. Достаем только те тики, которые "дошли" (прошло latency_ms)
        while let Some((timestamp, delayed_prices)) = buffer.front() {
            if timestamp.elapsed() >= self.latency {
                let p = delayed_prices.clone();
                buffer.pop_front();
                self.run_logic(p);
            } else {
                break;
            }
        }
    }

    fn run_logic(&self, prices: MarketPrices) {
        self.check_fills_and_timeouts(&prices);
        
        let mut port = self.portfolio.lock().unwrap();
        let skew = port.up_shares - port.down_shares;

        // ПРАВИЛО: Жесткий ребаланс при перекосе > 30 акций
        if skew >= HEDGE_SIZE {
            self.execute_trade(&mut port, Side::Down, prices.down_ask, skew.abs(), "Taker-Hedge");
            return;
        } else if skew <= -HEDGE_SIZE {
            self.execute_trade(&mut port, Side::Up, prices.up_ask, skew.abs(), "Taker-Hedge");
            return;
        }

        // ПРАВИЛО: Если одна нога уже есть (например, Maker зацепило), 
        // а вторая не закрыта и цена улетает (> 0.05 разница от идеальной пары)
        self.check_emergency_cover(&mut port, &prices);

        // ПРАВИЛО: Основная Maker стратегия (парная)
        self.manage_adaptive_maker(&port, &prices);
    }

    fn check_fills_and_timeouts(&self, prices: &MarketPrices) {
        let mut orders = self.active_limit_orders.lock().unwrap();
        let mut port = self.portfolio.lock().unwrap();

        orders.retain(|ord| {
            // Лимитка живет 5 секунд
            if ord.created_at.elapsed() > Duration::from_secs(5) { return false; }

            let fill = match ord.side {
                Side::Up => prices.up_bid < ord.price && prices.up_bid > 0.0,
                Side::Down => prices.down_bid < ord.price && prices.down_bid > 0.0,
            };

            if fill {
                self.execute_trade(&mut port, ord.side, ord.price, ord.shares, "Maker");
                return false;
            }
            true
        });
    }

    fn manage_adaptive_maker(&self, port: &Portfolio, prices: &MarketPrices) {
        let mut orders = self.active_limit_orders.lock().unwrap();
        if !orders.is_empty() { return; }

        if (port.up_spent + port.down_spent) >= VIRTUAL_BALANCE { return; }

        // ПРАВИЛО 1: Первая покупка там, где BestBid < 0.49
        if port.up_shares == 0.0 && port.down_shares == 0.0 {
            if prices.up_bid >= 0.49 && prices.down_bid >= 0.49 { return; }
        }

        // Проверка на возможность входа парой < 0.99 (Maker вход)
        let potential_pair_cost = (prices.up_bid + 0.01) + (prices.down_bid + 0.01);
        if potential_pair_cost >= 0.99 { return; }

        // ПРАВИЛО: Адаптация под конкуренцию (100 акций)
        let up_comp_high = prices.up_bid_size > 100.0;
        let down_comp_high = prices.down_bid_size > 100.0;

        // Ставим UP
        let up_price = if up_comp_high { prices.up_bid + 0.01 } else { prices.up_bid };
        orders.push(VirtualOrder { side: Side::Up, price: up_price, shares: SIZE, created_at: Instant::now() });

        // Ставим DOWN
        let down_price = if down_comp_high { prices.down_bid + 0.01 } else { prices.down_bid };
        orders.push(VirtualOrder { side: Side::Down, price: down_price, shares: SIZE, created_at: Instant::now() });
    }

    fn check_emergency_cover(&self, port: &mut Portfolio, prices: &MarketPrices) {
        let skew = port.up_shares - port.down_shares;
        if skew.abs() < 1.0 { return; } // Баланс в норме

        if skew > 0.0 { // Есть лишний UP, нужно докупить DOWN
            let current_pair_cost = port.up_avg() + prices.down_ask;
            // ПРАВИЛО: Если цена улетает или средняя пары становится > 1.05
            if current_pair_cost > 1.05 || prices.down_ask > 0.90 {
                self.execute_trade(port, Side::Down, prices.down_ask, skew, "Taker-Emergency");
            }
        } else { // Есть лишний DOWN, нужно докупить UP
            let current_pair_cost = port.down_avg() + prices.up_ask;
            if current_pair_cost > 1.05 || prices.up_ask > 0.90 {
                self.execute_trade(port, Side::Up, prices.up_ask, skew.abs(), "Taker-Emergency");
            }
        }
    }

    fn execute_trade(&self, port: &mut Portfolio, side: Side, price: f64, shares: f64, t_type: &str) {
        if price <= 0.0 || shares <= 0.0 { return; }

        let cost = price * shares;
        if (port.up_spent + port.down_spent + cost) > VIRTUAL_BALANCE {
            return; 
        }

        match side {
            Side::Up => { port.up_shares += shares; port.up_spent += price * shares; }
            Side::Down => { port.down_shares += shares; port.down_spent += price * shares; }
        }

        if t_type.contains("Maker") { port.maker_trades += 1; } 
        else { port.taker_trades += 1; }

        let record = TradeRecord {
            time: Utc::now().format("%H:%M:%S").to_string(),
            side: format!("{:?}", side),
            trade_type: t_type.to_string(),
            price, shares, cost: price * shares,
        };
        self.reporter.log_trade(&record);
        
        println!("🚀 {} {} @ {:.2} | Skew: {:.1} | PairAvg: {:.3}", 
            t_type, record.side, price, port.up_shares - port.down_shares, port.total_avg());
    }

    pub fn finalize(&self, final_prices: &MarketPrices) {
        let port = self.portfolio.lock().unwrap();
        let winner = if final_prices.up_bid > 0.5 { Side::Up } else { Side::Down };
        let winning_shares = if winner == Side::Up { port.up_shares } else { port.down_shares };
        let total_spent = port.up_spent + port.down_spent;
        let pnl = winning_shares - total_spent;

        // 1. Логируем финал в файл события
        let summary = format!(
            "\n=== FINAL REPORT (Latency: {:?}) ===\n\
            Winner: {:?}\n\
            Shares: UP {:.1}, DOWN {:.1}\n\
            Trades: Maker {}, Taker {}\n\
            Investment: ${:.2}, Final Value: ${:.2}\n\
            PnL: ${:.2} ({:.2}%)\n",
            self.latency, winner, port.up_shares, port.down_shares,
            port.maker_trades, port.taker_trades,
            total_spent, winning_shares, pnl, (pnl / total_spent) * 100.0
        );
        
        self.reporter.log_raw(&summary);

        // 2. Обновляем ГЛОБАЛЬНУЮ статку
        self.reporter.update_global_stats(pnl, total_spent, VIRTUAL_BALANCE);
    }
}
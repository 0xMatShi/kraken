use std::fs::{self, OpenOptions};
use std::io::{Write, Read};
use crate::models::{TradeRecord, GlobalSummary};

pub struct Reporter {
    file_path: String,
    global_path: String,
}

impl Reporter {
    pub fn new(slug: &str) -> Self {
        let _ = fs::create_dir_all("results");
        let file_path = format!("results/{}.txt", slug);
        let global_path = "results/global_summary.json".to_string();

        let reporter = Self {
            file_path,
            global_path,
        };

        // ВЫЗЫВАЕМ ИНИЦИАЛИЗАЦИЮ СРАЗУ ПРИ СОЗДАНИИ
        reporter.init_event_log(slug);

        reporter
    }

    // Инициализация файла события (без изменений, просто добавил flush)
    fn init_event_log(&self, slug: &str) {
        if !std::path::Path::new(&self.file_path).exists() {
            let mut file = fs::File::create(&self.file_path).unwrap();
            let header = format!("=== Trade Log for {} ===\n\n", slug);
            let table_head = format!("{:<20} | {:<5} | {:<15} | {:<8} | {:<10} | {:<8}\n", 
                "Timestamp", "Side", "Type", "Price", "Shares", "Cost");
            file.write_all(header.as_bytes()).unwrap();
            file.write_all(table_head.as_bytes()).unwrap();
            file.write_all("-".repeat(75).as_bytes()).unwrap();
            file.write_all(b"\n").unwrap();
        }
    }

    pub fn log_trade(&self, trade: &TradeRecord) {
        let mut file = OpenOptions::new().append(true).open(&self.file_path).unwrap();
        let line = format!("{:<20} | {:<5} | {:<15} | {:.2}     | {:<10.2} | ${:<8.2}\n",
            trade.time, trade.side, trade.trade_type, trade.price, trade.shares, trade.cost);
        file.write_all(line.as_bytes()).unwrap();
    }

    pub fn log_raw(&self, text: &str) {
        let mut file = OpenOptions::new().append(true).open(&self.file_path).unwrap();
        file.write_all(text.as_bytes()).unwrap();
    }

    // --- НОВАЯ ЛОГИКА ГЛОБАЛЬНОЙ СТАТИСТИКИ ---
    pub fn update_global_stats(&self, event_pnl: f64, total_spent: f64, virtual_balance: f64) {
        // 1. Читаем старую статку или создаем новую
        let mut stats: GlobalSummary = if let Ok(mut file) = fs::File::open(&self.global_path) {
            let mut content = String::new();
            file.read_to_string(&mut content).unwrap();
            serde_json::from_str(&content).unwrap_or_default()
        } else {
            GlobalSummary::default()
        };

        // 2. Обновляем показатели
        stats.total_pnl += event_pnl;
        stats.total_events += 1;
        if event_pnl > 0.0 { stats.wins += 1; }
        
        if event_pnl < stats.min_pnl { stats.min_pnl = event_pnl; }
        if event_pnl > stats.max_pnl { stats.max_pnl = event_pnl; }

        // 3. Считаем винрейт и проценты для красивого вывода
        let win_rate = (stats.wins as f64 / stats.total_events as f64) * 100.0;
        let pnl_vs_balance = (event_pnl / virtual_balance) * 100.0;
        let pnl_vs_spent = if total_spent > 0.0 { (event_pnl / total_spent) * 100.0 } else { 0.0 };

        // 4. Сохраняем обратно в JSON
        let json = serde_json::to_string_pretty(&stats).unwrap();
        fs::write(&self.global_path, json).unwrap();
        
        let summary = format!(
            "\n🌍 GLOBAL STATS UPDATED\n\
            Win Rate: {:.2}% | Total Events: {}\n\
            Event PnL: ${:.2} ({:.2}% of Balance | {:.2}% of Spent)\n\
            Total PnL: ${:.2} | Max: ${:.2} | Min: ${:.2}\n
            ", 
            win_rate, stats.total_events, event_pnl, pnl_vs_balance, pnl_vs_spent, stats.total_pnl, stats.max_pnl, stats.min_pnl
        );

        self.log_raw(&summary);

    }
}
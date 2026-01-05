use std::fs;
use std::io::Read;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use crate::models::{TradeRecord, GlobalSummary};

enum LogMessage {
    Trade(TradeRecord),
    Raw(String),
}

pub struct Reporter {
    #[allow(dead_code)]
    file_path: String,
    global_path: String,
    log_tx: mpsc::UnboundedSender<LogMessage>,
}

impl Reporter {
    pub fn new(slug: &str) -> Self {
        let _ = fs::create_dir_all("results");
        let file_path = format!("results/{}.txt", slug);
        let global_path = "results/global_summary.json".to_string();

        let (log_tx, mut log_rx) = mpsc::unbounded_channel::<LogMessage>();

        let file_path_clone = file_path.clone();
        let slug_owned = slug.to_string();

        tokio::spawn(async move {
            Self::async_init_event_log(&file_path_clone, &slug_owned).await;

            while let Some(msg) = log_rx.recv().await {
                match msg {
                    LogMessage::Trade(trade) => {
                        Self::async_log_trade(&file_path_clone, &trade).await;
                    }
                    LogMessage::Raw(text) => {
                        Self::async_log_raw(&file_path_clone, &text).await;
                    }
                }
            }
        });

        Self {
            file_path,
            global_path,
            log_tx,
        }
    }

    async fn async_init_event_log(file_path: &str, slug: &str) {
        if !tokio::fs::try_exists(file_path).await.unwrap_or(false) {
            let mut file = tokio::fs::File::create(file_path).await.unwrap();
            let header = format!("=== Trade Log for {} ===\n\n", slug);
            let table_head = format!("{:<20} | {:<5} | {:<15} | {:<8} | {:<10} | {:<8}\n",
                "Timestamp", "Side", "Type", "Price", "Shares", "Cost");
            file.write_all(header.as_bytes()).await.unwrap();
            file.write_all(table_head.as_bytes()).await.unwrap();
            file.write_all("-".repeat(75).as_bytes()).await.unwrap();
            file.write_all(b"\n").await.unwrap();
            file.flush().await.unwrap();
        }
    }

    pub fn log_trade(&self, trade: &TradeRecord) {
        let _ = self.log_tx.send(LogMessage::Trade(trade.clone()));
    }

    async fn async_log_trade(file_path: &str, trade: &TradeRecord) {
        if let Ok(mut file) = OpenOptions::new().append(true).open(file_path).await {
            let line = format!("{:<20} | {:<5} | {:<15} | {:.2}     | {:<10.2} | ${:<8.2}\n",
                trade.time, trade.side, trade.trade_type, trade.price, trade.shares, trade.cost);
            let _ = file.write_all(line.as_bytes()).await;
            let _ = file.flush().await;
        }
    }

    pub fn log_raw(&self, text: &str) {
        let _ = self.log_tx.send(LogMessage::Raw(text.to_string()));
    }

    async fn async_log_raw(file_path: &str, text: &str) {
        if let Ok(mut file) = OpenOptions::new().append(true).open(file_path).await {
            let _ = file.write_all(text.as_bytes()).await;
            let _ = file.flush().await;
        }
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
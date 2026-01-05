use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::collections::HashSet;
use chrono::Utc;
use crate::models::{Portfolio, Side, TradeRecord, MarketPrices};
use crate::report::Reporter;

use polymarket_client_sdk::clob::Client;
use polymarket_client_sdk::auth::Normal;
use polymarket_client_sdk::auth::state::Authenticated;
use polymarket_client_sdk::clob::types::{OrderType, Side as PolySide};
use polymarket_client_sdk::types::Decimal;
use alloy::signers::local::PrivateKeySigner;

const MAX_BALANCE: f64 = 15.0;
const SIZE: f64 = 3.0;
const HEDGE_SIZE: f64 = 6.0;

pub struct RealEngine {
    portfolio: Mutex<Portfolio>,
    reporter: Reporter,
    client: Client<Authenticated<Normal>>,
    signer: PrivateKeySigner,
    up_token: Arc<str>,
    down_token: Arc<str>,
    seen_trades: Mutex<HashSet<String>>,
    seen_orders: Mutex<HashSet<String>>,
    active_order_ids: Mutex<HashSet<String>>,
}

impl RealEngine {
    pub fn new(
        slug: &str,
        client: Client<Authenticated<Normal>>,
        signer: PrivateKeySigner,
        up_token: String,
        down_token: String
    ) -> Self {
        Self {
            portfolio: Mutex::new(Portfolio::default()),
            reporter: Reporter::new(slug),
            client,
            signer,
            up_token: Arc::from(up_token.as_str()),
            down_token: Arc::from(down_token.as_str()),
            seen_trades: Mutex::new(HashSet::new()),
            seen_orders: Mutex::new(HashSet::new()),
            active_order_ids: Mutex::new(HashSet::new()),
        }
    }

    // Округление до 2 знаков (минимальный тик-размер 0.01)
    fn round_price(price: f64) -> f64 {
        (price * 100.0).round() / 100.0
    }

    // Методы для работы с активными ордерами
    pub fn add_order_id(&self, order_id: String) {
        let mut orders = self.active_order_ids.lock().unwrap();
        orders.insert(order_id);
    }

    pub fn remove_order_id(&self, order_id: &str) {
        let mut orders = self.active_order_ids.lock().unwrap();
        orders.remove(order_id);
    }

    pub fn process_tick(&self, prices: MarketPrices) {
        self.run_logic(prices);
    }

    fn run_logic(&self, prices: MarketPrices) {
        let (up_shares, down_shares, up_spent, down_spent) = {
            let port = self.portfolio.lock().unwrap();
            (port.up_shares, port.down_shares, port.up_spent, port.down_spent)
        };

        let skew = up_shares - down_shares;

        if skew >= HEDGE_SIZE {
            self.execute_real_trade(Side::Down, prices.down_ask, skew.abs(), "Hedge");
            return;
        } else if skew <= -HEDGE_SIZE {
            self.execute_real_trade(Side::Up, prices.up_ask, skew.abs(), "Hedge");
            return;
        }

        self.check_emergency_cover(up_shares, down_shares, up_spent, down_spent, &prices);
        self.manage_adaptive_maker(up_spent, down_spent, &prices);
    }

    fn check_emergency_cover(&self, up_shares: f64, down_shares: f64, up_spent: f64, down_spent: f64, prices: &MarketPrices) {
        let skew = up_shares - down_shares;
        if skew.abs() < 1.0 { return; }

        let up_avg = if up_shares > 0.0 { up_spent / up_shares } else { 0.0 };
        let down_avg = if down_shares > 0.0 { down_spent / down_shares } else { 0.0 };

        if skew > 0.0 {
            if up_avg + prices.down_ask > 1.05 {
                self.execute_real_trade(Side::Down, prices.down_ask, skew, "Taker-Emergency");
            }
        } else {
            if down_avg + prices.up_ask > 1.05 {
                self.execute_real_trade(Side::Up, prices.up_ask, skew.abs(), "Taker-Emergency");
            }
        }
    }

    fn manage_adaptive_maker(&self, up_spent: f64, down_spent: f64, prices: &MarketPrices) {
        if (up_spent + down_spent) >= MAX_BALANCE { return; }

        // Проверяем, что обе стороны имеют валидные bid цены
        if prices.up_bid < 0.01 || prices.down_bid < 0.01 {
            return;
        }

        let potential_pair_cost = (prices.up_bid + 0.01) + (prices.down_bid + 0.01);
        if potential_pair_cost >= 0.99 { return; }

        let up_price = if prices.up_bid_size > 100.0 {
            Self::round_price(prices.up_bid + 0.01)
        } else {
            Self::round_price(prices.up_bid)
        };
        let down_price = if prices.down_bid_size > 100.0 {
            Self::round_price(prices.down_bid + 0.01)
        } else {
            Self::round_price(prices.down_bid)
        };

        // Дополнительная проверка после округления
        if up_price < 0.01 || down_price < 0.01 {
            return;
        }

        let up_token = Arc::clone(&self.up_token);
        let down_token = Arc::clone(&self.down_token);
        let client = self.client.clone();
        let signer = self.signer.clone();

        tokio::spawn(async move {
            // Конвертируем через строку с точным форматированием до 2 знаков
            let up_price_dec: Decimal = format!("{:.2}", up_price).parse().unwrap();
            let down_price_dec: Decimal = format!("{:.2}", down_price).parse().unwrap();
            let size_dec: Decimal = format!("{:.2}", SIZE).parse().unwrap();

            let order_up = client.limit_order()
                .token_id(&*up_token)
                .price(up_price_dec)
                .size(size_dec)
                .side(PolySide::Buy)
                .build().await.unwrap();

            let order_down = client.limit_order()
                .token_id(&*down_token)
                .price(down_price_dec)
                .size(size_dec)
                .side(PolySide::Buy)
                .build().await.unwrap();

            let signed_up = client.sign(&signer, order_up).await.unwrap();
            let signed_down = client.sign(&signer, order_down).await.unwrap();

            match client.post_orders(vec![signed_up, signed_down]).await {
                Ok(responses) => {
                    let mut order_ids = Vec::new();
                    for resp in responses.iter() {
                        if !resp.order_id.is_empty() {
                            order_ids.push(resp.order_id.clone());
                        }
                    }

                    tokio::time::sleep(Duration::from_secs(5)).await;

                    // Отменяем ордера
                    for id in order_ids {
                        let _ = client.cancel_order(&id).await;
                    }
                },
                Err(e) => eprintln!("❌ Ошибка размещения Maker ордеров: {}", e),
            }
        });
    }

    fn execute_real_trade(&self, side: Side, price: f64, shares: f64, t_type: &str) {
        let client = self.client.clone();
        let signer = self.signer.clone();
        let token_id = if side == Side::Up { Arc::clone(&self.up_token) } else { Arc::clone(&self.down_token) };
        let t_type_str = t_type.to_string();

        tokio::spawn(async move {
            // Округляем сумму до 2 знаков и конвертируем через строку
            let total_amount = ((price * shares) * 100.0).round() / 100.0;
            let amount_dec: Decimal = format!("{:.2}", total_amount).parse().unwrap();
            let usdc_amount = polymarket_client_sdk::clob::types::Amount::usdc(amount_dec).unwrap();

            let order = client.market_order()
                .token_id(&*token_id)
                .amount(usdc_amount)
                .side(PolySide::Buy)
                .order_type(OrderType::FAK)
                .build().await;

            match order {
                Ok(ord) => {
                    let signed = client.sign(&signer, ord).await.unwrap();
                    match client.post_order(signed).await {
                        Ok(_) => {}, // Успех - подтверждение придет через WsMessage::Trade
                        Err(e) => eprintln!("❌ Ошибка отправки Taker {}: {}", t_type_str, e),
                    }
                },
                Err(e) => eprintln!("❌ Ошибка создания Taker {}: {}", t_type_str, e),
            }
        });
    }

    // Trade события = TAKER сделки (market orders FAK)
    // Это подтверждение исполнения taker-hedge и taker-emergency ордеров
    pub fn handle_ws_trade(&self, trade_id: String, price: f64, size: f64, side: PolySide, asset_id: &str) {
        // Проверяем дубликаты
        {
            let mut seen = self.seen_trades.lock().unwrap();
            if seen.contains(&trade_id) {
                return;
            }
            seen.insert(trade_id.clone());
        }

        let mut port = self.portfolio.lock().unwrap();

        port.taker_trades += 1;

        let is_buy = matches!(side, PolySide::Buy);

        if asset_id == &*self.up_token {
            if is_buy {
                port.up_shares += size;
                port.up_spent += price * size;
            } else {
                port.up_shares -= size;
                port.up_spent -= price * size;
            }
        } else if asset_id == &*self.down_token {
            if is_buy {
                port.down_shares += size;
                port.down_spent += price * size;
            } else {
                port.down_shares -= size;
                port.down_spent -= price * size;
            }
        }

        let side_str = if is_buy { "BUY" } else { "SELL" };
        let token_str = if asset_id == &*self.up_token { "UP" } else { "DOWN" };

        println!("✅ TAKER FILLED: {} {} @ {:.3} | Size: {:.2} | Cost: ${:.2}",
            side_str, token_str, price, size, price * size);
        println!("💰 Portfolio: UP {:.1} | DOWN {:.1} | Skew {:.1}",
            port.up_shares, port.down_shares, port.up_shares - port.down_shares);

        let record = TradeRecord {
            time: Utc::now().format("%H:%M:%S").to_string(),
            side: if asset_id == &*self.up_token { "UP".to_string() } else { "DOWN".to_string() },
            trade_type: "Taker".to_string(),
            price,
            shares: size,
            cost: price * size,
        };
        self.reporter.log_trade(&record);
    }

    // Обработка событий ордеров (MAKER orders - limit orders)
    // PLACEMENT - ордер размещён
    // UPDATE - ордер частично/полностью исполнен (some of it is matched)
    // CANCELLATION - ордер отменён
    pub fn handle_ws_order(&self, order_id: String, msg_type: Option<String>, price: f64, side: PolySide, asset_id: &str, size_matched: Option<f64>) {
        // Создаем уникальный ключ: order_id + status
        let order_key = format!("{}:{:?}", order_id, msg_type);

        // Проверяем дубликаты
        {
            let mut seen = self.seen_orders.lock().unwrap();
            if seen.contains(&order_key) {
                return; // Молча игнорируем дубликаты
            }
            seen.insert(order_key);
        }

        let side_str = match side {
            PolySide::Buy => "BUY",
            PolySide::Sell => "SELL",
            _ => "UNKNOWN",
        };

        let token_str = if asset_id == &*self.up_token { "UP" } else { "DOWN" };

        match msg_type.as_deref() {
            Some("PLACEMENT") => {
                // Сохраняем ID нашего ордера
                self.add_order_id(order_id.clone());

                let msg = format!("📝 MAKER PLACED: {} {} @ {:.3} | ID: {}\n",
                    side_str, token_str, price, &order_id[..20]);
                println!("{}", msg.trim());
                self.reporter.log_raw(&msg);
            }
            Some("UPDATE") => {
                // UPDATE = частичное или полное исполнение MAKER ордера
                if let Some(size) = size_matched {
                    let mut port = self.portfolio.lock().unwrap();
                    port.maker_trades += 1;

                    let is_buy = matches!(side, PolySide::Buy);

                    if asset_id == &*self.up_token {
                        if is_buy {
                            port.up_shares += size;
                            port.up_spent += price * size;
                        } else {
                            port.up_shares -= size;
                            port.up_spent -= price * size;
                        }
                    } else if asset_id == &*self.down_token {
                        if is_buy {
                            port.down_shares += size;
                            port.down_spent += price * size;
                        } else {
                            port.down_shares -= size;
                            port.down_spent -= price * size;
                        }
                    }

                    println!("✅ MAKER FILLED: {} {} @ {:.3} | Size: {:.2} | Cost: ${:.2}",
                        side_str, token_str, price, size, price * size);
                    println!("💰 Portfolio: UP {:.1} | DOWN {:.1} | Skew {:.1}",
                        port.up_shares, port.down_shares, port.up_shares - port.down_shares);

                    let record = TradeRecord {
                        time: Utc::now().format("%H:%M:%S").to_string(),
                        side: if asset_id == &*self.up_token { "UP".to_string() } else { "DOWN".to_string() },
                        trade_type: "Maker".to_string(),
                        price,
                        shares: size,
                        cost: price * size,
                    };
                    self.reporter.log_trade(&record);
                }
            }
            Some("CANCELLATION") => {
                // Удаляем из активных
                self.remove_order_id(&order_id);

                let msg = format!("❌ MAKER CANCELLED: {} {} @ {:.3} | ID: {}\n",
                    side_str, token_str, price, &order_id[..20]);
                println!("{}", msg.trim());
                self.reporter.log_raw(&msg);
            }
            _ => {
                // Другие типы событий
            }
        }
    }

    pub fn finalize(&self, final_prices: &MarketPrices) {
        let port = self.portfolio.lock().unwrap();
        let winner = if final_prices.up_bid > 0.5 { Side::Up } else { Side::Down };
        let winning_shares = if winner == Side::Up { port.up_shares } else { port.down_shares };
        let total_spent = port.up_spent + port.down_spent;
        let pnl = winning_shares - total_spent;

        let summary = format!(
            "\n=== FINAL REPORT ===\nWinner: {:?}\n\
            Shares Held: {:.2}\nCost Basis: ${:.2}\nPnL: ${:.2}\n\
            Maker Trades: {}\nTaker Trades: {}\n\
            ",
            winner, winning_shares, total_spent, pnl, port.maker_trades, port.taker_trades
        );
        self.reporter.log_raw(&summary);

        self.reporter.update_global_stats(pnl, total_spent, MAX_BALANCE);

    }
}
use std::sync::Mutex;
use std::time::Duration;
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
    up_token: String,
    down_token: String,
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
            up_token,
            down_token,
        }
    }

    // Округление до 2 знаков (минимальный тик-размер 0.01)
    fn round_price(price: f64) -> f64 {
        (price * 100.0).round() / 100.0
    }

    pub fn process_tick(&self, prices: MarketPrices) {
        self.run_logic(prices);
    }

    fn run_logic(&self, prices: MarketPrices) {
        let mut port = self.portfolio.lock().unwrap();
        let skew = port.up_shares - port.down_shares;

        if skew >= HEDGE_SIZE {
            port.taker_trades += 1;
            drop(port); 
            self.execute_real_trade(Side::Down, prices.down_ask, skew.abs(), "Taker-Hedge");
            return;
        } else if skew <= -HEDGE_SIZE {
            port.taker_trades += 1;
            drop(port);
            self.execute_real_trade(Side::Up, prices.up_ask, skew.abs(), "Taker-Hedge");
            return;
        }

        self.check_emergency_cover(&mut port, &prices);
        self.manage_adaptive_maker(&port, &prices);
    }

    fn check_emergency_cover(&self, port: &mut Portfolio, prices: &MarketPrices) {
        let skew = port.up_shares - port.down_shares;
        if skew.abs() < 1.0 { return; }

        if skew > 0.0 {
            if port.up_avg() + prices.down_ask > 1.05 {
                self.execute_real_trade(Side::Down, prices.down_ask, skew, "Taker-Emergency");
            }
        } else {
            if port.down_avg() + prices.up_ask > 1.05 {
                self.execute_real_trade(Side::Up, prices.up_ask, skew.abs(), "Taker-Emergency");
            }
        }
    }

    fn manage_adaptive_maker(&self, port: &Portfolio, prices: &MarketPrices) {
        if (port.up_spent + port.down_spent) >= MAX_BALANCE { return; }

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

        let up_token = self.up_token.clone();
        let down_token = self.down_token.clone();
        let client = self.client.clone();
        let signer = self.signer.clone();

        tokio::spawn(async move {
            // Конвертируем через строку для точного представления с 2 знаками после запятой
            let up_price_dec = format!("{:.2}", up_price).parse::<Decimal>().unwrap();
            let down_price_dec = format!("{:.2}", down_price).parse::<Decimal>().unwrap();
            let size_dec = Decimal::from_f64_retain(SIZE).unwrap();

            let order_up = client.limit_order()
                .token_id(&up_token)
                .price(up_price_dec)
                .size(size_dec)
                .side(PolySide::Buy)
                .build().await.unwrap();

            let order_down = client.limit_order()
                .token_id(&down_token)
                .price(down_price_dec)
                .size(size_dec)
                .side(PolySide::Buy)
                .build().await.unwrap();

            let signed_up = client.sign(&signer, order_up).await.unwrap();
            let signed_down = client.sign(&signer, order_down).await.unwrap();

            match client.post_orders(vec![signed_up, signed_down]).await {
                Ok(responses) => {
                    println!("📥 Maker: размещены ордера UP@{:.2}, DOWN@{:.2}", up_price, down_price);
                    
                    let mut order_ids = Vec::new();
                    for resp in responses {
                        // ИСПРАВЛЕНИЕ: В SDK order_id часто просто String, а не Option.
                        // Если компилятор ругался на if let Some(id), значит там уже String.
                        if !resp.order_id.is_empty() {
                            order_ids.push(resp.order_id);
                        }
                    }

                    tokio::time::sleep(Duration::from_secs(5)).await;

                    if !order_ids.is_empty() {
                        for id in order_ids {
                            // ИСПРАВЛЕНИЕ: передаем &str
                            match client.cancel_order(&id).await {
                                Ok(_) => { },
                                Err(_) => { } // Игнорируем ошибки отмены (например, уже исполнен)
                            }
                        }
                    }
                },
                Err(e) => eprintln!("❌ Ошибка размещения Maker ордеров: {}", e),
            }
        });
    }

    fn execute_real_trade(&self, side: Side, price: f64, shares: f64, t_type: &str) {
        let client = self.client.clone();
        let signer = self.signer.clone();
        let token_id = if side == Side::Up { self.up_token.clone() } else { self.down_token.clone() };
        let t_type_str = t_type.to_string();

        tokio::spawn(async move {
            // Округляем сумму до 2 знаков перед конвертацией в Decimal
            let total_amount = ((price * shares) * 100.0).round() / 100.0;
            let amount_dec = Decimal::from_f64_retain(total_amount).unwrap();
            let usdc_amount = polymarket_client_sdk::clob::types::Amount::usdc(amount_dec).unwrap();
            
            let order = client.market_order()
                .token_id(&token_id)
                .amount(usdc_amount)
                .side(PolySide::Buy)
                .order_type(OrderType::FAK)
                .build().await;

            match order {
                Ok(ord) => {
                    let signed = client.sign(&signer, ord).await.unwrap();
                    match client.post_order(signed).await {
                        Ok(_) => println!("🚀 Taker {} {} отправлен по {:.2}", t_type_str, format!("{:?}", side), price),
                        Err(e) => eprintln!("❌ Ошибка отправки Taker ордера: {}", e),
                    }
                },
                Err(e) => eprintln!("❌ Ошибка создания Taker ордера: {}", e),
            }
        });
    }

    // ИСПРАВЛЕНИЕ: Принимаем примитивы, а не SDK структуры
    pub fn handle_ws_trade(&self, price: f64, size: f64, side: PolySide, asset_id: &str) {
        let mut port = self.portfolio.lock().unwrap();
        
        port.maker_trades += 1;

        let is_buy = match side {
            PolySide::Buy => true,
            PolySide::Sell => false,
            _ => {
                // Если придет что-то странное (чего быть не должно), считаем за Sell или игнорируем
                eprintln!("⚠️ Получен неизвестный тип стороны сделки!");
                false
            }
        };

        if asset_id == self.up_token {
            if is_buy {
                port.up_shares += size;
                port.up_spent += price * size;
            } else {
                port.up_shares -= size;
                port.up_spent -= price * size;
            }
        } else if asset_id == self.down_token {
            if is_buy {
                port.down_shares += size;
                port.down_spent += price * size;
            } else {
                port.down_shares -= size;
                port.down_spent -= price * size;
            }
        }

        println!("💰 Balance Update: UP {:.1} | DOWN {:.1} | Skew {:.1}", 
            port.up_shares, port.down_shares, port.up_shares - port.down_shares);
        
        let record = TradeRecord {
            time: Utc::now().format("%H:%M:%S").to_string(),
            side: if asset_id == self.up_token { "UP".to_string() } else { "DOWN".to_string() },
            trade_type: "WS-Fill".to_string(),
            price,
            shares: size,
            cost: price * size,
        };
        self.reporter.log_trade(&record);
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
use std::sync::{Arc, Mutex};
use std::collections::{HashSet, HashMap};
use crate::models::{Portfolio, Side, MarketPrices};
use crate::config::TradingConfig;
use crate::ui::{self, UiState};

use polymarket_client_sdk::clob::Client;
use polymarket_client_sdk::auth::Normal;
use polymarket_client_sdk::auth::state::Authenticated;
use polymarket_client_sdk::clob::types::{OrderType, Side as PolySide};
use polymarket_client_sdk::types::Decimal;
use alloy::signers::local::PrivateKeySigner;
use tracing::{info, warn, error};
use uuid::Uuid;
// use chrono::{TimeDelta, Utc};


pub struct RealEngine {
    portfolio: Mutex<Portfolio>,
    client: Client<Authenticated<Normal>>,
    signer: PrivateKeySigner,
    up_token: Arc<str>,
    down_token: Arc<str>,
    seen_trades: Mutex<HashSet<String>>,
    seen_orders: Mutex<HashSet<String>>,
    active_order_ids: Mutex<HashSet<String>>,
    active_orders_info: Mutex<HashMap<String, (f64, bool, f64, f64)>>,  // order_id -> (price, is_up, original_size, accumulated_filled)
    our_api_key: Uuid,
    // hedging_in_progress: Arc<Mutex<bool>>,
    config: TradingConfig,
    // Защита от чрезмерного размещения лимиток
    last_maker_prices: Mutex<Option<(f64, f64)>>,  // (up_price, down_price)
    maker_placement_count: Mutex<u32>,              // Счетчик размещений с текущими ценами
    // UI state для отображения портфолио и контроля режима торговли
    ui_state: UiState,
}

impl RealEngine {
    pub fn new(
        client: Client<Authenticated<Normal>>,
        signer: PrivateKeySigner,
        up_token: String,
        down_token: String,
        our_api_key: Uuid,
        config: TradingConfig,
        ui_state: UiState,
    ) -> Self {
        Self {
            portfolio: Mutex::new(Portfolio::default()),
            client,
            signer,
            up_token: Arc::from(up_token.as_str()),
            down_token: Arc::from(down_token.as_str()),
            seen_trades: Mutex::new(HashSet::new()),
            seen_orders: Mutex::new(HashSet::new()),
            active_order_ids: Mutex::new(HashSet::new()),
            active_orders_info: Mutex::new(HashMap::new()),
            our_api_key,
            // hedging_in_progress: Arc::new(Mutex::new(false)),
            config,
            last_maker_prices: Mutex::new(None),
            maker_placement_count: Mutex::new(0),
            ui_state,
        }
    }

    // Обновить UI с текущим состоянием портфолио
    fn update_ui_portfolio(&self) {
        let port = self.portfolio.lock().unwrap();
        ui::update_portfolio(&self.ui_state, port.clone());
    }

    // Округление до 2 знаков (минимальный тик-размер 0.01)
    fn round_price(price: f64) -> f64 {
        (price * 100.0).round() / 100.0
    }

    // ВРЕМЕННО ОТКЛЮЧЕНО: Методы для работы с флагом хеджирования
    // fn is_hedging(&self) -> bool {
    //     *self.hedging_in_progress.lock().unwrap()
    // }

    // fn set_hedging(&self, value: bool) {
    //     *self.hedging_in_progress.lock().unwrap() = value;
    // }

    // ВРЕМЕННО ОТКЛЮЧЕНО: Таймер для автоматического сброса флага хеджирования
    // fn start_hedging_timer(&self) {
    //     let hedging_flag = Arc::clone(&self.hedging_in_progress);
    //     tokio::spawn(async move {
    //         tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
    //         *hedging_flag.lock().unwrap() = false;
    //         info!("⏰ Таймер хеджирования истек. Снимаем блокировку");
    //     });
    // }

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
        // Проверяем режим торговли - если торговля выключена, не размещаем ордера
        let trading_enabled = {
            let state = self.ui_state.lock().unwrap();
            state.trading_enabled
        };

        if !trading_enabled {
            return;
        }

        // ВРЕМЕННО ОТКЛЮЧЕНО: Проверка флага хеджирования
        // if self.is_hedging() {
        //     return;
        // }
        self.run_logic(prices);
    }

    fn run_logic(&self, prices: MarketPrices) {
        let (up_shares, down_shares, up_spent, down_spent) = {
            let port = self.portfolio.lock().unwrap();
            (port.up_shares, port.down_shares, port.up_spent, port.down_spent)
        };

        if (up_spent + down_spent) >= self.config.max_balance {
            return;
        }

        let _skew = up_shares - down_shares;

        // ВРЕМЕННО ОТКЛЮЧЕНО: Логика хеджирования
        // if skew >= self.config.hedge_size {
        //     // Устанавливаем флаг хеджирования
        //     self.set_hedging(true);
        //     // Запускаем таймер для автоматического сброса флага через 3 секунды
        //     self.start_hedging_timer();
        //     // Вычисляем сколько ордеров нужно для закрытия перекоса
        //     let num_orders = ((skew - self.config.hedge_size) / self.config.size).ceil() as i32;
        //     // Отправляем все нужные ордера
        //     for _ in 0..num_orders {
        //         self.execute_hedge_trade(Side::Down);
        //     }
        //     return;
        // } else if skew <= -self.config.hedge_size {
        //     // Устанавливаем флаг хеджирования
        //     self.set_hedging(true);
        //     // Запускаем таймер для автоматического сброса флага через 3 секунды
        //     self.start_hedging_timer();
        //     // Вычисляем сколько ордеров нужно для закрытия перекоса
        //     let num_orders = ((skew.abs() - self.config.hedge_size) / self.config.size).ceil() as i32;
        //     // Отправляем все нужные ордера
        //     for _ in 0..num_orders {
        //         self.execute_hedge_trade(Side::Up);
        //     }
        //     return;
        // }

        self.manage_adaptive_maker(&prices);
    }

    fn manage_adaptive_maker(&self, prices: &MarketPrices) {

        // Проверяем, что обе стороны имеют валидные bid цены
        if prices.up_bid < 0.01 || prices.down_bid < 0.01 {
            return;
        }

        // СТРАТЕГИЯ 1
        // let potential_pair_cost = prices.up_bid + prices.down_bid;
        // if potential_pair_cost >= 1.00 { return; }

        // // Стратегия размещения в зависимости от спреда
        // let (up_price, down_price) = if potential_pair_cost >= 0.99 {
        //     // Спред 1с: встаем в очередь с обеих сторон, только если сайз <= 100
        //     if prices.up_bid_size > 100.0 || prices.down_bid_size > 100.0 {
        //         return; // Не размещаем, если хотя бы в одной очереди > 100 акций
        //     }
        //     (Self::round_price(prices.up_bid), Self::round_price(prices.down_bid))
        // } else if potential_pair_cost >= 0.98 {
        //     // Спред 2с: старая логика - где конкуренция больше, там новый best_bid
        //     if prices.up_bid_size > prices.down_bid_size {
        //         // UP конкуренция больше → встаем новым best_bid
        //         // DOWN конкуренция меньше → встаем в очередь, но проверяем лимит
        //         if prices.down_bid_size > 100.0 {
        //             return; // Не размещаем, если в очереди DOWN > 100 акций
        //         }
        //         (Self::round_price(prices.up_bid + 0.01), Self::round_price(prices.down_bid))
        //     } else if prices.down_bid_size > prices.up_bid_size {
        //         // DOWN конкуренция больше → встаем новым best_bid
        //         // UP конкуренция меньше → встаем в очередь, но проверяем лимит
        //         if prices.up_bid_size > 100.0 {
        //             return; // Не размещаем, если в очереди UP > 100 акций
        //         }
        //         (Self::round_price(prices.up_bid), Self::round_price(prices.down_bid + 0.01))
        //     } else {
        //         // Конкуренция равна - встаем в очередь с обеих сторон
        //         if prices.up_bid_size > 100.0 || prices.down_bid_size > 100.0 {
        //             return; // Не размещаем, если хотя бы в одной очереди > 100 акций
        //         }
        //         (Self::round_price(prices.up_bid), Self::round_price(prices.down_bid))
        //     }
        // } else {
        //     // Спред 3с и больше: встаем новым best_bid с обеих сторон
        //     (Self::round_price(prices.up_bid + 0.01), Self::round_price(prices.down_bid + 0.01))
        // };

        // СТРАТЕГИЯ 2: Упрощенная - всегда новый best_bid если есть спред
        let potential_pair_cost = (prices.up_bid + 0.01) + (prices.down_bid + 0.01);
        if potential_pair_cost >= 1.00 { return; }

        // Всегда размещаем новым best_bid, не смотрим на размер очереди
        let up_price = Self::round_price(prices.up_bid + 0.01);
        let down_price = Self::round_price(prices.down_bid + 0.01);

        // Дополнительная проверка после округления
        if up_price < 0.01 || down_price < 0.01 { return; }

        // Защита от чрезмерного размещения лимиток
        {
            let mut last_prices = self.last_maker_prices.lock().unwrap();
            let mut count = self.maker_placement_count.lock().unwrap();

            if let Some((last_up, last_down)) = *last_prices {
                // Проверяем, совпадают ли цены с предыдущими
                if (last_up - up_price).abs() < 0.001 && (last_down - down_price).abs() < 0.001 {
                    // Цены не изменились, увеличиваем счетчик
                    *count += 1;

                    // Если уже было 3 или больше размещений с такими же ценами, скипаем
                    if *count > 2 { return; }
                } else {
                    // Цены изменились, сбрасываем счетчик
                    *count = 1;
                    *last_prices = Some((up_price, down_price));
                }
            } else {
                // Первое размещение
                *count = 1;
                *last_prices = Some((up_price, down_price));
            }
        }

        let up_token = Arc::clone(&self.up_token);
        let down_token = Arc::clone(&self.down_token);
        let client = self.client.clone();
        let signer = self.signer.clone();
        let size = self.config.size;

        tokio::spawn(async move {
            // Конвертируем через строку с точным форматированием до 2 знаков
            let up_price_dec: Decimal = format!("{:.2}", up_price).parse().unwrap();
            let down_price_dec: Decimal = format!("{:.2}", down_price).parse().unwrap();
            let size_dec: Decimal = format!("{:.2}", size).parse().unwrap();
            
            // let expiration = Utc::now() + TimeDelta::seconds(65);

            let order_up = client.limit_order()
                .token_id(&*up_token)
                .price(up_price_dec)
                .size(size_dec)
                .side(PolySide::Buy)
                .order_type(OrderType::GTC)
                // .expiration(expiration)
                .build().await.unwrap();

            let order_down = client.limit_order()
                .token_id(&*down_token)
                .price(down_price_dec)
                .size(size_dec)
                .side(PolySide::Buy)
                .order_type(OrderType::GTC)
                // .expiration(expiration)
                .build().await.unwrap();

            let signed_up = client.sign(&signer, order_up).await.unwrap();
            let signed_down = client.sign(&signer, order_down).await.unwrap();

            match client.post_orders(vec![signed_up, signed_down]).await {
                Ok(_responses) => {},
                Err(e) => error!("❌ Ошибка размещения Maker ордеров: {}", e),
            }
        });
    }

    // ВРЕМЕННО ОТКЛЮЧЕНО: Функция хеджирования
    // fn execute_hedge_trade(&self, side: Side) {
    //     let client: Client<Authenticated<Normal>> = self.client.clone();
    //     let signer = self.signer.clone();
    //     let token_id = if side == Side::Up { Arc::clone(&self.up_token) } else { Arc::clone(&self.down_token) };
    //     let size = self.config.size;

    //     tokio::spawn(async move {
    //         // Конвертируем через строку с точным форматированием до 2 знаков
    //         let price_dec: Decimal = format!("{:.2}", 0.99).parse().unwrap();
    //         let size_dec: Decimal = format!("{:.2}", size).parse().unwrap();

    //         let order = client.limit_order()
    //             .token_id(&*token_id)
    //             .price(price_dec)
    //             .size(size_dec)
    //             .side(PolySide::Buy)
    //             .build().await.unwrap();

    //         let signed = client.sign(&signer, order).await.unwrap();

    //         match client.post_order(signed).await {
    //             Ok(response) => {
    //                 if !response.order_id.is_empty() {
    //                     info!("🎯 Taker Hedge отправлен");
    //                 }
    //             },
    //             Err(e) => error!("❌ Ошибка отправки Taker Hedge: {}", e),
    //         }
    //     });
    // }

    // Trade события = TAKER сделки (market orders FAK)
    // Это подтверждение исполнения taker-hedge и taker-emergency ордеров
    // ВАЛИДАЦИЯ: Проверяем по trade_owner
    pub fn handle_ws_trade(
        &self,
        trade_id: String,
        price: f64,
        size: f64,
        side: PolySide,
        asset_id: &str,
        trade_owner: Option<Uuid>,
    ) {
        // ПРОВЕРКА 1: trade_owner должен совпадать с нашим API key
        let owner_matches = trade_owner.map_or(false, |owner| owner == self.our_api_key);

        if !owner_matches { return; }

        // Проверяем дубликаты (дополнительная защита)
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
                port.up_total_placed += size;  // Тейкер тоже считается как "выставленный"
            } else {
                port.up_shares -= size;
                port.up_spent -= price * size;
            }
        } else if asset_id == &*self.down_token {
            if is_buy {
                port.down_shares += size;
                port.down_spent += price * size;
                port.down_total_placed += size;  // Тейкер тоже считается как "выставленный"
            } else {
                port.down_shares -= size;
                port.down_spent -= price * size;
            }
        }

        let side_str = if is_buy { "BUY" } else { "SELL" };
        let token_str = if asset_id == &*self.up_token { "UP" } else { "DOWN" };

        info!("✅ TAKER FILLED: {} {} @ {:.3} | Size: {:.2} | Cost: ${:.2}",
            side_str, token_str, price, size, price * size);
        info!("💰 Portfolio: UP {:.1} | DOWN {:.1} | Skew {:.1}",
            port.up_shares, port.down_shares, port.up_shares - port.down_shares);

        // ВРЕМЕННО ОТКЛЮЧЕНО: Проверка перекоса и снятие флага хеджирования
        // let current_skew = port.up_shares - port.down_shares;

        // Освобождаем мьютекс портфеля перед работой с флагом
        drop(port);

        // Обновляем UI
        self.update_ui_portfolio();

        // // Если перекос выровнялся (меньше HEDGE_SIZE), снимаем флаг хеджирования
        // if current_skew.abs() < self.config.hedge_size && self.is_hedging() {
        //     self.set_hedging(false);
        //     info!("✅ Перекос выровнен. Возобновляем нормальную торговлю");
        // }
    }

    // Обработка событий ордеров (MAKER orders - limit orders)
    // PLACEMENT - ордер размещён
    // UPDATE - ордер частично/полностью исполнен (some of it is matched)
    // CANCELLATION - ордер отменён
    pub fn handle_ws_order(&self, order_id: String, msg_type: Option<String>, price: f64, side: PolySide, asset_id: &str, size_matched: Option<f64>, original_size: Option<f64>) {
        // Дедупликация только для PLACEMENT и CANCELLATION
        // UPDATE события НЕ дедуплицируются, так как ордер может исполняться частями
        if msg_type.as_deref() != Some("UPDATE") {
            let order_key = format!("{}:{:?}", order_id, msg_type);
            let mut seen = self.seen_orders.lock().unwrap();
            if seen.contains(&order_key) {
                return; // Молча игнорируем дубликаты PLACEMENT/CANCELLATION
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

                // Определяем is_up (UP или DOWN токен)
                let is_up = asset_id == &*self.up_token;

                // Сохраняем информацию об ордере (цена, сторона, original_size и accumulated_filled = 0.0)
                if let Some(size) = original_size {
                    let mut orders_info = self.active_orders_info.lock().unwrap();
                    orders_info.insert(order_id.clone(), (price, is_up, size, 0.0));
                }

                // Обновляем UI - добавляем цену в список наших bid prices
                ui::add_our_bid_price(&self.ui_state, is_up, price);

                // Отслеживаем выставленные shares
                if let Some(size) = original_size {
                    let mut port = self.portfolio.lock().unwrap();
                    if is_up {
                        port.up_total_placed += size;
                    } else {
                        port.down_total_placed += size;
                    }
                    drop(port);
                    self.update_ui_portfolio();
                }

                info!("📝 MAKER PLACED: {} {} @ {:.3}",
                    side_str, token_str, price);
            }
            Some("UPDATE") => {
                // UPDATE = частичное или полное исполнение MAKER ордера
                if let Some(size) = size_matched {
                    // Получаем информацию об ордере и накапливаем исполнение
                    let mut orders_info = self.active_orders_info.lock().unwrap();

                    if let Some((order_price, is_up, original_size, accumulated_filled)) = orders_info.get_mut(&order_id) {
                        // Добавляем текущее исполнение к накопленному
                        *accumulated_filled += size;

                        info!("📊 MAKER PARTIAL FILL: {} {} @ {:.3} | Filled: {:.2}/{:.2}",
                            side_str, token_str, price, *accumulated_filled, *original_size);

                        // Проверяем, полностью ли исполнен ордер (с погрешностью 0.01)
                        if (*accumulated_filled - *original_size).abs() < 0.01 || *accumulated_filled >= *original_size {
                            // Ордер полностью исполнен - сохраняем данные для удаления
                            let final_price = *order_price;
                            let final_is_up = *is_up;

                            // Удаляем из HashMap
                            orders_info.remove(&order_id);
                            drop(orders_info); // Освобождаем мьютекс

                            // Удаляем часики из UI
                            ui::remove_our_bid_price(&self.ui_state, final_is_up, final_price);
                            info!("🔔 ОРДЕР ПОЛНОСТЬЮ ИСПОЛНЕН: {} {} @ {:.3}", side_str, token_str, price);
                        } else {
                            drop(orders_info); // Освобождаем мьютекс если ордер еще не полностью исполнен
                        }
                    } else {
                        drop(orders_info); // Освобождаем мьютекс если ордер не найден
                    }

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

                    info!("✅ MAKER FILLED: {} {} @ {:.3} | Size: {:.2} | Cost: ${:.2}",
                        side_str, token_str, price, size, price * size);
                    info!("💰 Portfolio: UP {:.1} | DOWN {:.1} | Skew {:.1}",
                        port.up_shares, port.down_shares, port.up_shares - port.down_shares);

                    drop(port);
                    // Обновляем UI
                    self.update_ui_portfolio();
                }
            }
            Some("CANCELLATION") => {
                // Удаляем из активных
                self.remove_order_id(&order_id);

                // Получаем информацию о цене и стороне из нашего хранилища
                let order_info = {
                    let mut orders_info = self.active_orders_info.lock().unwrap();
                    orders_info.remove(&order_id)
                };

                // Обновляем UI - удаляем цену из списка наших bid prices
                if let Some((order_price, is_up, _original_size, _accumulated)) = order_info {
                    ui::remove_our_bid_price(&self.ui_state, is_up, order_price);
                }

                warn!("❌ MAKER CANCELLED: {} {} @ {:.3}",
                    side_str, token_str, price);
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

        info!("=== FINAL REPORT ===");
        info!("Winner: {:?}", winner);
        info!("Shares Held: {:.2}", winning_shares);
        info!("Cost Basis: ${:.2}", total_spent);
        info!("PnL: ${:.2}", pnl);
        info!("Maker Trades: {}", port.maker_trades);
        info!("Taker Trades: {}", port.taker_trades);
    }
}
mod core;
mod models;
pub mod ui;
mod utils;
mod websocket;

use chrono::{DateTime, Utc};
use core::RealEngine;
use std::io::{self, Write};
use std::sync::Arc;
use utils::{AutoScanner, Config};
use websocket::market::DataStream;
use websocket::user::UserStream;

use polymarket_client_sdk::clob::ws::Client as WsClient;
use polymarket_client_sdk::clob::{Client, Config as ClobConfig};
use polymarket_client_sdk::data::Client as DataClient;
use polymarket_client_sdk::data::types::request::PositionsRequest;

use tokio::time::Duration;

/// Интерактивное меню редактирования конфига
fn edit_config_menu(config: &mut Config) -> anyhow::Result<()> {
    loop {
        // Очищаем экран и показываем текущие параметры
        print!("\x1B[2J\x1B[1;1H");
        println!("=== Edit Config ===");
        println!("1. max_balance = {}", config.trading.max_balance);
        println!("2. size = {}", config.trading.size);
        println!("3. max_size_side = {}", config.trading.max_size_side);
        println!("4. chain_links = {}", config.trading.chain_links);
        println!(
            "5. seconds_before_start = {}",
            config.trading.seconds_before_start
        );
        println!(
            "6. seconds_until_end = {}",
            config.trading.seconds_until_end
        );
        println!("7. legs_strategy = \"{}\"", config.trading.legs_strategy);
        println!("8. Save & Exit");
        println!("9. Cancel (without saving)");
        print!("> ");
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;

        match input.trim() {
            "1" => {
                // Редактируем max_balance
                print!("Enter new max_balance: ");
                io::stdout().flush()?;
                let mut value = String::new();
                io::stdin().read_line(&mut value)?;
                match value.trim().parse::<f64>() {
                    Ok(v) if v > 0.0 => {
                        config.trading.max_balance = v;
                        println!("✅ max_balance updated to {}", v);
                    }
                    _ => {
                        println!("❌ Invalid value");
                    }
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            "2" => {
                // Редактируем size
                print!("Enter new size: ");
                io::stdout().flush()?;
                let mut value = String::new();
                io::stdin().read_line(&mut value)?;
                match value.trim().parse::<f64>() {
                    Ok(v) if v > 0.0 => {
                        config.trading.size = v;
                        println!("✅ size updated to {}", v);
                    }
                    _ => {
                        println!("❌ Invalid value");
                    }
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            "3" => {
                // Редактируем max_size_side
                print!("Enter new max_size_side: ");
                io::stdout().flush()?;
                let mut value = String::new();
                io::stdin().read_line(&mut value)?;
                match value.trim().parse::<f64>() {
                    Ok(v) if v > 0.0 => {
                        config.trading.max_size_side = v;
                        println!("✅ max_size_side updated to {}", v);
                    }
                    _ => {
                        println!("❌ Invalid value");
                    }
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            "4" => {
                // Редактируем chain_links
                print!("Enter new chain_links: ");
                io::stdout().flush()?;
                let mut value = String::new();
                io::stdin().read_line(&mut value)?;
                match value.trim().parse::<u32>() {
                    Ok(v) if v >= 1 => {
                        config.trading.chain_links = v;
                        println!("✅ chain_links updated to {}", v);
                    }
                    _ => {
                        println!("❌ Invalid value (must be >= 1)");
                    }
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            "5" => {
                // Редактируем seconds_before_start
                print!("Enter new seconds_before_start: ");
                io::stdout().flush()?;
                let mut value = String::new();
                io::stdin().read_line(&mut value)?;
                match value.trim().parse::<i64>() {
                    Ok(v) if v >= 0 => {
                        config.trading.seconds_before_start = v;
                        println!("✅ seconds_before_start updated to {}", v);
                    }
                    _ => {
                        println!("❌ Invalid value");
                    }
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            "6" => {
                // Редактируем seconds_until_end
                print!("Enter new seconds_until_end: ");
                io::stdout().flush()?;
                let mut value = String::new();
                io::stdin().read_line(&mut value)?;
                match value.trim().parse::<i64>() {
                    Ok(v) if v >= 0 => {
                        config.trading.seconds_until_end = v;
                        println!("✅ seconds_until_end updated to {}", v);
                    }
                    _ => {
                        println!("❌ Invalid value");
                    }
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            "7" => {
                // Редактируем legs_strategy
                print!("\x1B[2J\x1B[1;1H");
                println!("Select legs_strategy:");
                println!("1. strong");
                println!("2. weak");
                println!("3. both");
                println!("4. cumulative");
                println!("5. math");
                print!("> ");
                io::stdout().flush()?;
                let mut strategy_input = String::new();
                io::stdin().read_line(&mut strategy_input)?;
                match strategy_input.trim() {
                    "1" => {
                        config.trading.legs_strategy = "strong".to_string();
                        println!("✅ legs_strategy updated to \"strong\"");
                    }
                    "2" => {
                        config.trading.legs_strategy = "weak".to_string();
                        println!("✅ legs_strategy updated to \"weak\"");
                    }
                    "3" => {
                        config.trading.legs_strategy = "both".to_string();
                        println!("✅ legs_strategy updated to \"both\"");
                    }
                    "4" => {
                        config.trading.legs_strategy = "cumulative".to_string();
                        println!("✅ legs_strategy updated to \"cumulative\"");
                    }
                    "5" => {
                        config.trading.legs_strategy = "math".to_string();
                        println!("✅ legs_strategy updated to \"math\"");
                    }
                    _ => {
                        println!("❌ Invalid choice");
                    }
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            "8" => {
                // Сохраняем и выходим
                config.save()?;
                println!("✅ Config saved to config.toml");
                std::thread::sleep(std::time::Duration::from_secs(1));
                return Ok(());
            }
            "9" => {
                // Отменяем без сохранения
                println!("❌ Changes discarded");
                std::thread::sleep(std::time::Duration::from_secs(1));
                return Ok(());
            }
            _ => continue,
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    // Создаем UI state для захвата логов
    let ui_state = ui::new_ui_state();

    // Инициализация логгера (сохраняем guard для поддержания записи в файл)
    let _log_guard = utils::logger::init_logger()?;

    // Загружаем торговую конфигурацию (mut для редактирования)
    let mut app_config = Config::load()?;
    tracing::info!(
        "Конфигурация загружена: MAX_BALANCE={:.1}, SIZE={:.1}, SECONDS_BEFORE_START={}, LEGS_STRATEGY={}",
        app_config.trading.max_balance,
        app_config.trading.size,
        app_config.trading.seconds_before_start,
        app_config.trading.legs_strategy
    );

    // Загружаем переменные окружения
    let env_config = utils::load_env::load_env_config()?;

    // Создаем клиента Polymarket
    let client = Client::new("https://clob.polymarket.com", ClobConfig::default())?
        .authentication_builder(&env_config.signer)
        .funder(env_config.funder_address)
        .signature_type(polymarket_client_sdk::clob::types::SignatureType::GnosisSafe)
        .authenticate()
        .await?;

    tracing::info!("Аутентификация CLOB успешна");

    let ws_client =
        WsClient::default().authenticate(env_config.credentials, env_config.funder_address)?;

    tracing::info!("Аутентификация WsUser успешна");

    let scanner = AutoScanner::new();

    // Главный цикл выбора
    loop {
        // Очищаем экран и показываем меню
        print!("\x1B[2J\x1B[1;1H");
        println!("MMDNA-Bot");
        println!("1. Start | 2. Edit Config | 3. Exit");
        print!("> ");
        io::stdout().flush().unwrap();

        let mut input = String::new();
        io::stdin().read_line(&mut input).unwrap();

        match input.trim() {
            "1" => {} // Start
            "2" => {
                // Edit Config
                if let Err(e) = edit_config_menu(&mut app_config) {
                    tracing::error!("Ошибка редактирования конфига: {}", e);
                }
                continue;
            }
            "3" => break,  // Exit
            _ => continue, // Invalid input
        }

        // Меню выбора монеты
        print!("\x1B[2J\x1B[1;1H");
        println!("Select Coin:");
        println!("1. BTC | 2. ETH | 3. SOL | 4. XRP");
        print!("> ");
        io::stdout().flush().unwrap();

        let mut coin_input = String::new();
        io::stdin().read_line(&mut coin_input).unwrap();

        let coin = match coin_input.trim().parse::<u8>() {
            Ok(idx) => {
                if let Some(c) = models::Coin::from_index(idx) {
                    c
                } else {
                    continue; // Invalid coin index
                }
            }
            Err(_) => continue,
        };

        // Меню выбора типа рынка
        print!("\x1B[2J\x1B[1;1H");
        println!("Select Market Type:");
        println!("1. 15min | 2. 1hour");
        print!("> ");
        io::stdout().flush().unwrap();

        let mut market_type_input = String::new();
        io::stdin().read_line(&mut market_type_input).unwrap();

        let market_type = match market_type_input.trim() {
            "1" => models::MarketType::FifteenMin,
            "2" => models::MarketType::OneHour,
            _ => continue,
        };

        // Автоматический цикл торговли для выбранной монеты
        loop {
            // Ищем подходящий рынок
            if let Some(target) = scanner
                .find_next_target(
                    market_type.slug_prefix_for_coin(coin),
                    0.0,
                    market_type.max_minutes(),
                )
                .await
            {
                // Парсим дату окончания
                let end_date = target
                    .end_date
                    .parse::<DateTime<Utc>>()
                    .unwrap_or(Utc::now());
                let total_seconds = market_type.total_seconds();

                // Устанавливаем информацию о событии в UI
                ui::set_event_info(
                    &ui_state,
                    target.title.clone(),
                    target.slug.clone(),
                    end_date,
                    total_seconds,
                );
                ui::clear_our_bid_prices(&ui_state);
                ui::clear_open_orders(&ui_state);
                ui::set_config(
                    &ui_state,
                    app_config.trading.max_balance,
                    app_config.trading.size,
                    app_config.trading.max_size_side,
                    app_config.trading.chain_links,
                    app_config.trading.seconds_before_start,
                    app_config.trading.seconds_until_end,
                    app_config.trading.legs_strategy.clone(),
                );

                // Создаем реальный движок
                let engine = Arc::new(RealEngine::new(
                    client.clone(),
                    env_config.signer.clone(),
                    target.up_token.clone(),
                    target.down_token.clone(),
                    env_config.api_key,
                    app_config.trading.clone(),
                    ui_state.clone(),
                ));

                let market_stream = DataStream::new(
                    target.up_token.clone(),
                    target.down_token.clone(),
                    engine.clone(),
                    env_config.ws_market_url.clone(),
                    ui_state.clone(),
                );

                let user_stream = UserStream::new(engine.clone(), ws_client.clone());

                // Задача периодического опроса REST API позиций (раз в 10 секунд)
                let data_client = DataClient::default();
                let positions_req = PositionsRequest::builder()
                    .user(env_config.funder_address)
                    .build();
                let up_token_rest = target.up_token.clone();
                let down_token_rest = target.down_token.clone();
                let ui_state_rest = ui_state.clone();
                let positions_task = async move {
                    loop {
                        match data_client.positions(&positions_req).await {
                            Ok(positions) => {
                                let mut rest = crate::models::RestPositions::default();
                                for pos in &positions {
                                    if pos.asset == *up_token_rest {
                                        rest.up_shares = pos.size.try_into().unwrap_or(0.0);
                                        rest.up_avg_price = pos.avg_price.try_into().unwrap_or(0.0);
                                    } else if pos.asset == *down_token_rest {
                                        rest.down_shares = pos.size.try_into().unwrap_or(0.0);
                                        rest.down_avg_price =
                                            pos.avg_price.try_into().unwrap_or(0.0);
                                    }
                                }
                                ui_state_rest.lock().unwrap().rest_positions = rest;
                            }
                            Err(e) => tracing::warn!("REST positions error: {}", e),
                        }
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                };

                tracing::info!("Запуск торговой сессии...");

                // Инициализируем терминал для TUI
                let mut terminal = ui::init_terminal()?;

                // Флаг для отслеживания причины завершения
                let user_exit_requested = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let user_exit_flag = user_exit_requested.clone();

                // Запускаем UI рендеринг и торговую логику параллельно
                let ui_state_clone = ui_state.clone();
                let trading_task = async {
                    tokio::select! {
                        res = market_stream.start_stream(target.end_date) => {
                            if let Err(e) = res { tracing::error!("Market stream died: {}", e); }
                        }
                        res = user_stream.start_stream() => {
                            if let Err(e) = res { tracing::error!("User stream died: {}", e); }
                        }
                    }
                };

                // UI loop
                let engine_for_ui = engine.clone();
                let ui_task = async {
                    loop {
                        // Получаем текущий режим и состояние hedge для check_key_action
                        let (current_mode, hedge_input) = {
                            let state = ui_state_clone.lock().unwrap();
                            (state.trading_mode, state.hedge_input_state.clone())
                        };

                        // Проверка нажатых клавиш и обработка ввода hedge
                        let (key_action, new_hedge_state) =
                            ui::check_key_action(current_mode, &hedge_input);

                        // Обновляем состояние hedge если изменилось
                        if let Some(new_state) = new_hedge_state {
                            let mut state = ui_state_clone.lock().unwrap();
                            state.hedge_input_state = new_state;
                        }

                        match key_action {
                            ui::KeyAction::Exit => {
                                user_exit_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                                ui::stop_ui(&ui_state_clone);
                                break;
                            }
                            ui::KeyAction::ToggleTrading
                            | ui::KeyAction::ActivateCancelling
                            | ui::KeyAction::ToggleHedge => {
                                // Переключаем режим
                                ui::switch_trading_mode(&ui_state_clone, key_action);
                            }
                            ui::KeyAction::CancelAllOrders => {
                                // Отменяем все ордера и переключаем режим
                                let should_cancel =
                                    ui::switch_trading_mode(&ui_state_clone, key_action);
                                if should_cancel {
                                    let engine_cancel = engine_for_ui.clone();
                                    tokio::spawn(async move {
                                        engine_cancel.cancel_all_orders().await;
                                    });
                                }
                            }
                            ui::KeyAction::RequestUpHedge => {
                                // Начинаем ввод количества UP
                                let mut state = ui_state_clone.lock().unwrap();
                                state.hedge_input_state =
                                    ui::HedgeInputState::RequestingUp(String::new());
                            }
                            ui::KeyAction::RequestDownHedge => {
                                // Начинаем ввод количества DOWN
                                let mut state = ui_state_clone.lock().unwrap();
                                state.hedge_input_state =
                                    ui::HedgeInputState::RequestingDown(String::new());
                            }
                            ui::KeyAction::ConfirmHedge => {
                                // Разместить hedge ордер
                                let (side, input) = {
                                    let state = ui_state_clone.lock().unwrap();
                                    match &state.hedge_input_state {
                                        ui::HedgeInputState::RequestingUp(s) => {
                                            (Some(crate::models::Side::Up), s.clone())
                                        }
                                        ui::HedgeInputState::RequestingDown(s) => {
                                            (Some(crate::models::Side::Down), s.clone())
                                        }
                                        _ => (None, String::new()),
                                    }
                                };

                                if let Some(side) = side {
                                    // Парсим введенное количество
                                    match input.parse::<f64>() {
                                        Ok(size) if size > 0.0 => {
                                            tracing::info!(
                                                "✅ Размещаем hedge ордер: {:?} Size: {:.2}",
                                                side,
                                                size
                                            );

                                            // Размещаем hedge ордер
                                            let engine_hedge = engine_for_ui.clone();
                                            tokio::spawn(async move {
                                                crate::core::streams::place_hedge_order(
                                                    &engine_hedge,
                                                    side,
                                                    size,
                                                );
                                            });

                                            // Очищаем состояние ввода
                                            let mut state = ui_state_clone.lock().unwrap();
                                            state.hedge_input_state = ui::HedgeInputState::None;
                                        }
                                        _ => {
                                            tracing::warn!(
                                                "❌ Некорректное количество: '{}'",
                                                input
                                            );
                                        }
                                    }
                                }
                            }
                            ui::KeyAction::CancelHedgeInput => {
                                // Отменяем ввод, остаемся в режиме Hedge
                                let mut state = ui_state_clone.lock().unwrap();
                                state.hedge_input_state = ui::HedgeInputState::None;
                                tracing::info!("❌ Ввод hedge отменен");
                            }
                            ui::KeyAction::ToggleObiPanel => {
                                let mut state = ui_state_clone.lock().unwrap();
                                state.show_obi_panel = !state.show_obi_panel;
                            }
                            ui::KeyAction::None => {}
                        }

                        // Проверяем, закончилась ли сессия
                        {
                            let state = ui_state_clone.lock().unwrap();
                            if !state.is_running {
                                break;
                            }
                        }

                        // Рендерим UI
                        terminal
                            .draw(|frame| {
                                ui::render(frame, &ui_state_clone);
                            })
                            .ok();

                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                };

                // Запускаем всё в параллель
                tokio::select! {
                    _ = trading_task => {
                        // Торговля завершилась
                        ui::stop_ui(&ui_state);
                    }
                    _ = ui_task => {
                        // UI завершился (может быть нажата q или событие закончилось)
                    }
                    _ = positions_task => {}
                }

                // Восстанавливаем терминал
                ui::restore_terminal(&mut terminal)?;

                // Проверяем, запросил ли пользователь выход
                if user_exit_requested.load(std::sync::atomic::Ordering::SeqCst) {
                    tracing::info!("Пользователь запросил выход в меню");
                    // Сбрасываем состояние UI, но сохраняем trading_mode
                    {
                        let mut state = ui_state.lock().unwrap();
                        let trading_mode = state.trading_mode;
                        *state = ui::UiStateInner::default();
                        state.trading_mode = trading_mode;
                    }
                    break; // Выход в главное меню
                }

                // Сбрасываем состояние UI для следующей сессии, но сохраняем trading_mode
                {
                    let mut state = ui_state.lock().unwrap();
                    let trading_mode = state.trading_mode;
                    *state = ui::UiStateInner::default();
                    state.trading_mode = trading_mode;
                }

                tracing::info!("Событие завершено, ищем следующее...");
                // Цикл продолжится и найдет следующее событие
            } else {
                // Если событие не найдено, выходим из автоматического цикла
                tracing::warn!("Не найдено подходящих событий");
                break;
            }
        }
    }
    Ok(())
}

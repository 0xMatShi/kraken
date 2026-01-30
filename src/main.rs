mod models;
mod websocket;
mod core;
mod utils;
pub mod ui;

use std::io::{self, Write};
use std::sync::Arc;
use utils::{AutoScanner, Config};
use websocket::market::DataStream;
use websocket::user::UserStream;
use core::RealEngine;
use chrono::{DateTime, Utc};

use polymarket_client_sdk::clob::{Client, Config as ClobConfig};
use polymarket_client_sdk::clob::ws::{Client as WsClient};

use tokio::time::Duration;


#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    // Создаем UI state для захвата логов
    let ui_state = ui::new_ui_state();

    // Инициализация логгера (сохраняем guard для поддержания записи в файл)
    let _log_guard = utils::logger::init_logger()?;

    // Загружаем торговую конфигурацию
    let app_config = Config::load()?;
    tracing::info!("Конфигурация загружена: MAX_BALANCE={:.1}, SIZE={:.1}, SECONDS_BEFORE_START={}",
        app_config.trading.max_balance,
        app_config.trading.size,
        app_config.trading.seconds_before_start
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

    let ws_client = WsClient::default().authenticate(env_config.credentials, env_config.funder_address)?;

    tracing::info!("Аутентификация WsUser успешна");

    let scanner = AutoScanner::new();

    // Главный цикл выбора
    loop {
        // Очищаем экран и показываем меню
        print!("\x1B[2J\x1B[1;1H");
        println!("MMDNA-Bot");
        println!("1. Start | 2. Exit");
        print!("> "); io::stdout().flush().unwrap();

        let mut input = String::new();
        io::stdin().read_line(&mut input).unwrap();

        match input.trim() {
            "1" => {},          // Start
            "2" => break,       // Exit
            _ => continue,      // Invalid input
        }

        // Меню выбора монеты
        print!("\x1B[2J\x1B[1;1H");
        println!("Select Coin:");
        println!("1. BTC | 2. ETH | 3. SOL | 4. XRP");
        print!("> "); io::stdout().flush().unwrap();

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

        // Автоматический цикл торговли для выбранной монеты
        loop {
            // Ищем подходящий рынок
            if let Some(target) = scanner.find_next_target(coin.slug_prefix(), 0.0, 15.0).await {
                // Парсим дату окончания
                let end_date = target.end_date.parse::<DateTime<Utc>>().unwrap_or(Utc::now());
                let total_seconds = 900; // Фиксированная длительность события: 15 минут

                // Устанавливаем информацию о событии в UI
                ui::set_event_info(&ui_state, target.title.clone(), target.slug.clone(), end_date, total_seconds);
                ui::clear_our_bid_prices(&ui_state);
                ui::clear_open_orders(&ui_state);
                ui::set_config(&ui_state, app_config.trading.max_balance, app_config.trading.size, app_config.trading.seconds_before_start);

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

                        // Если в режиме ввода hedge, обрабатываем ввод цифр
                        if !matches!(hedge_input, ui::HedgeInputState::None) {
                            if let Some(new_state) = ui::handle_hedge_input(&hedge_input) {
                                let mut state = ui_state_clone.lock().unwrap();
                                state.hedge_input_state = new_state;
                            }
                        }

                        // Проверка нажатых клавиш
                        let key_action = ui::check_key_action(current_mode, &hedge_input);
                        match key_action {
                            ui::KeyAction::Exit => {
                                user_exit_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                                ui::stop_ui(&ui_state_clone);
                                break;
                            }
                            ui::KeyAction::ToggleTrading | ui::KeyAction::ActivateCancelling | ui::KeyAction::ToggleHedge => {
                                // Переключаем режим
                                ui::switch_trading_mode(&ui_state_clone, key_action);
                            }
                            ui::KeyAction::CancelAllOrders => {
                                // Отменяем все ордера и переключаем режим
                                let should_cancel = ui::switch_trading_mode(&ui_state_clone, key_action);
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
                                state.hedge_input_state = ui::HedgeInputState::RequestingUp(String::new());
                            }
                            ui::KeyAction::RequestDownHedge => {
                                // Начинаем ввод количества DOWN
                                let mut state = ui_state_clone.lock().unwrap();
                                state.hedge_input_state = ui::HedgeInputState::RequestingDown(String::new());
                            }
                            ui::KeyAction::ConfirmHedge => {
                                // Разместить hedge ордер
                                let (side, input) = {
                                    let state = ui_state_clone.lock().unwrap();
                                    match &state.hedge_input_state {
                                        ui::HedgeInputState::RequestingUp(s) => (Some(crate::models::Side::Up), s.clone()),
                                        ui::HedgeInputState::RequestingDown(s) => (Some(crate::models::Side::Down), s.clone()),
                                        _ => (None, String::new()),
                                    }
                                };

                                if let Some(side) = side {
                                    // Парсим введенное количество
                                    match input.parse::<f64>() {
                                        Ok(size) if size > 0.0 => {
                                            tracing::info!("✅ Размещаем hedge ордер: {:?} Size: {:.2}", side, size);

                                            // Размещаем hedge ордер
                                            let engine_hedge = engine_for_ui.clone();
                                            tokio::spawn(async move {
                                                crate::core::streams::place_hedge_order(&engine_hedge, side, size);
                                            });

                                            // Очищаем состояние ввода
                                            let mut state = ui_state_clone.lock().unwrap();
                                            state.hedge_input_state = ui::HedgeInputState::None;
                                        }
                                        _ => {
                                            tracing::warn!("❌ Некорректное количество: '{}'", input);
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
                        terminal.draw(|frame| {
                            ui::render(frame, &ui_state_clone);
                        }).ok();

                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                };

                // Запускаем оба в параллель
                tokio::select! {
                    _ = trading_task => {
                        // Торговля завершилась
                        ui::stop_ui(&ui_state);
                    }
                    _ = ui_task => {
                        // UI завершился (может быть нажата q или событие закончилось)
                    }
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

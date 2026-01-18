mod models;
mod websocket;
mod core;
mod utils;
pub mod ui;

use std::io::{self, Write};
use std::sync::Arc;
use utils::{AutoScanner, PriceTracker, Config};
use websocket::market::DataStream;
use websocket::user::UserStream;
use websocket::coinbase::CoinbaseStream;
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
    tracing::info!("Конфигурация загружена: MAX_BALANCE={:.1}, SIZE={:.1}, CHEAP_LIMIT={:.1}, EXPIRATION={}s",
        app_config.trading.max_balance,
        app_config.trading.size,
        app_config.trading.cheap_limit,
        app_config.trading.expiration_seconds
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

    // Загружаем price tracker
    let mut price_tracker = PriceTracker::load();

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

                // Извлекаем timestamp из slug события
                let event_timestamp = utils::price_tracker::extract_timestamp_from_slug(&target.slug);

                // Получаем price to beat для этого события
                let price_to_beat = if let Some(ts) = event_timestamp {
                    price_tracker.get_price_to_beat(coin, ts)
                } else {
                    None
                };

                // Устанавливаем информацию о событии в UI
                ui::set_event_info(&ui_state, target.title.clone(), target.slug.clone(), end_date, total_seconds);
                ui::set_price_to_beat(&ui_state, price_to_beat);
                ui::clear_our_bid_prices(&ui_state);
                ui::clear_open_orders(&ui_state);

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

                // Создаем Coinbase stream для получения текущих цен
                let coinbase_stream = CoinbaseStream::new(coin, ui_state.clone());

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
                        res = coinbase_stream.start_stream() => {
                            if let Err(e) = res { tracing::error!("Coinbase stream died: {}", e); }
                        }
                    }
                };

                // UI loop
                let ui_task = async {
                    loop {
                        // Проверка нажатых клавиш
                        match ui::check_key_action() {
                            ui::KeyAction::Exit => {
                                user_exit_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                                ui::stop_ui(&ui_state_clone);
                                break;
                            }
                            ui::KeyAction::ToggleTrading => {
                                ui::toggle_trading(&ui_state_clone);
                                // Логируем изменение состояния
                                let trading_enabled = {
                                    let state = ui_state_clone.lock().unwrap();
                                    state.trading_enabled
                                };
                                if trading_enabled {
                                    tracing::info!("🟢 ТОРГОВЛЯ ВКЛЮЧЕНА - режим реальной торговли");
                                } else {
                                    tracing::info!("🔴 ТОРГОВЛЯ ВЫКЛЮЧЕНА - режим наблюдения (DRY RUN)");
                                }
                            }
                            ui::KeyAction::ScrollHistoryUp => {
                                ui::scroll_history_up(&ui_state_clone);
                            }
                            ui::KeyAction::ScrollHistoryDown => {
                                ui::scroll_history_down(&ui_state_clone);
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

                // Сохраняем последнюю цену для следующего события
                if let Some(ts) = event_timestamp {
                    let last_price = {
                        let state = ui_state.lock().unwrap();
                        state.current_price
                    };

                    if let Some(price) = last_price {
                        price_tracker.set_last_price(coin, price, ts);
                        if let Err(e) = price_tracker.save() {
                            tracing::error!("Не удалось сохранить price tracker: {}", e);
                        }
                    }
                }

                // Проверяем, запросил ли пользователь выход
                if user_exit_requested.load(std::sync::atomic::Ordering::SeqCst) {
                    tracing::info!("Пользователь запросил выход в меню");
                    // Сбрасываем состояние UI, но сохраняем trading_enabled
                    {
                        let mut state = ui_state.lock().unwrap();
                        let trading_enabled = state.trading_enabled;
                        *state = ui::UiStateInner::default();
                        state.trading_enabled = trading_enabled;
                    }
                    break; // Выход в главное меню
                }

                // Сбрасываем состояние UI для следующей сессии, но сохраняем trading_enabled
                {
                    let mut state = ui_state.lock().unwrap();
                    let trading_enabled = state.trading_enabled;
                    *state = ui::UiStateInner::default();
                    state.trading_enabled = trading_enabled;
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

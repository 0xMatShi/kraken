mod models; mod trading; mod websocket; mod config; pub mod ui;
use std::io::{self, Write};
use std::sync::Arc;
use std::str::FromStr as _;
use trading::scanner::AutoScanner;
use websocket::market::DataStream;
use websocket::user::UserStream;
use trading::engine::RealEngine;
use chrono::{DateTime, Utc};

use alloy::signers::Signer as _;
use alloy::signers::local::PrivateKeySigner;
use alloy::primitives::Address;
use polymarket_client_sdk::clob::{Client, Config};
use polymarket_client_sdk::clob::ws::{Client as WsClient};
use polymarket_client_sdk::auth::Credentials;
use polymarket_client_sdk::{POLYGON, PRIVATE_KEY_VAR};
use uuid::Uuid;

use tracing_subscriber::fmt;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::fmt::time::OffsetTime;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;
use time::macros::format_description;
use tokio::time::Duration;


#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    // Создаем UI state для захвата логов
    let ui_state = ui::new_ui_state();

    // Настройка tracing логгера с кастомным форматом времени
    let timer = OffsetTime::new(
        time::UtcOffset::UTC,
        format_description!("[hour]:[minute]:[second].[subsecond digits:3]"),
    );

    // Фильтр логов: по умолчанию WARN для всех библиотек, INFO для нашего проекта
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| {
            EnvFilter::new("warn,mmdnca=info")
        });

    // Создаем неблокирующий файловый appender (один файл без ротации)
    let file_appender = tracing_appender::rolling::never("./logs", "app.log");
    let (non_blocking_file, _log_guard) = tracing_appender::non_blocking(file_appender);

    // Слой для записи в файл (без цветов)
    let file_layer = fmt::layer()
        .with_target(false)
        .with_thread_ids(false)
        .with_file(true)
        .with_line_number(true)
        .with_level(true)
        .with_ansi(false)
        .with_span_events(FmtSpan::NONE)
        .with_timer(timer)
        .with_writer(non_blocking_file);

    // UI лог слой для отправки логов в TUI
    let ui_log_layer = ui::UiLogLayer::new(ui_state.clone());

    // Объединяем слои и инициализируем
    tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(ui_log_layer)
        .init();

    // Загружаем торговую конфигурацию
    let app_config = config::Config::load()?;
    tracing::info!("Конфигурация загружена: MAX_BALANCE={:.1}, SIZE={:.1}, HEDGE_SIZE={:.1}",
        app_config.trading.max_balance,
        app_config.trading.size,
        app_config.trading.hedge_size
    );

    // 1. Инициализация аутентификации
    let api_key = Uuid::parse_str(&std::env::var("POLYMARKET_API_KEY")?)?;
    let api_secret = std::env::var("POLYMARKET_API_SECRET")?;
    let api_passphrase = std::env::var("POLYMARKET_API_PASSPHRASE")?;
    let private_key = std::env::var(PRIVATE_KEY_VAR).expect("Нужен PRIVATE_KEY_VAR в .env");
    let funder_addr_str = std::env::var("FUNDER_ADDRESS").expect("Нужен FUNDER_ADDRESS в .env");
    let funder_address: Address = funder_addr_str.parse()
    .expect("Неверный формат адреса в FUNDER_ADDRESS (должен начинаться с 0x...)");
    let ws_market_url = std::env::var("CLOB_WS_MARKET").expect("Нужен CLOB_WS_MARKET в .env");

    let signer = PrivateKeySigner::from_str(&private_key)?.with_chain_id(Some(POLYGON));

    let credentials = Credentials::new(api_key, api_secret, api_passphrase);

    // Создаем клиента Polymarket
    let client = Client::new("https://clob.polymarket.com", Config::default())?
        .authentication_builder(&signer)
        .funder(funder_address)
        .signature_type(polymarket_client_sdk::clob::types::SignatureType::GnosisSafe)
        .authenticate()
        .await?;

    tracing::info!("Аутентификация CLOB успешна");

    let ws_client = WsClient::default().authenticate(credentials, funder_address)?;

    tracing::info!("Аутентификация WsUser успешна");

    let scanner = AutoScanner::new();

    // Главный цикл выбора
    loop {
        // Очищаем экран и показываем меню
        print!("\x1B[2J\x1B[1;1H");
        println!("MMDNA-Bot");
        println!("1. Start | 2. Start(DR) | 3. Exit");
        print!("> "); io::stdout().flush().unwrap();

        let mut input = String::new();
        io::stdin().read_line(&mut input).unwrap();

        let dry_run = match input.trim() {
            "1" => false,       // Normal trading
            "2" => true,        // Dry run mode
            "3" => break,       // Exit
            _ => continue,      // Invalid input
        };

        // Ищем подходящий рынок
        if let Some(target) = scanner.find_next_target("btc-updown-15m", 0.0, 15.0).await {
            // Парсим дату окончания
            let end_date = target.end_date.parse::<DateTime<Utc>>().unwrap_or(Utc::now());
            let total_seconds = 900; // Фиксированная длительность события: 15 минут

            // Устанавливаем информацию о событии в UI
            ui::set_event_info(&ui_state, target.title.clone(), end_date, total_seconds);
            ui::set_dry_run(&ui_state, dry_run);

            // Создаем реальный движок
            let engine = Arc::new(RealEngine::new(
                client.clone(),
                signer.clone(),
                target.up_token.clone(),
                target.down_token.clone(),
                api_key,
                app_config.trading.clone(),
                ui_state.clone(),
                dry_run,
            ));

            if dry_run {
                tracing::info!("DRY RUN MODE - только наблюдение");
            }

            let market_stream = DataStream::new(
                target.up_token.clone(),
                target.down_token.clone(),
                engine.clone(),
                ws_market_url.clone(),
                ui_state.clone(),
            );

            let user_stream = UserStream::new(engine.clone(), ws_client.clone());

            tracing::info!("Запуск торговой сессии...");

            // Инициализируем терминал для TUI
            let mut terminal = ui::init_terminal()?;

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
            let ui_task = async {
                loop {
                    // Проверка выхода
                    if ui::check_exit_key() {
                        ui::stop_ui(&ui_state_clone);
                        break;
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
                    // UI завершился (нажата q)
                }
            }

            // Восстанавливаем терминал
            ui::restore_terminal(&mut terminal)?;

            // Сбрасываем состояние UI для следующей сессии
            {
                let mut state = ui_state.lock().unwrap();
                *state = ui::UiStateInner::default();
            }
        }
    }
    Ok(())
}

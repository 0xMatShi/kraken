mod models; mod scanner; mod websocket; mod engine; mod websocket_user;
use std::io::{self, Write};
use std::sync::Arc;
use std::str::FromStr as _;
use scanner::AutoScanner;
use websocket::DataStream;
use websocket_user::UserStream;
use engine::RealEngine;

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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    // Настройка tracing логгера с кастомным форматом времени
    let timer = OffsetTime::new(
        time::UtcOffset::UTC,
        format_description!("[hour]:[minute]:[second].[subsecond digits:3]"),
    );

    // Фильтр логов: по умолчанию WARN для всех библиотек, INFO для нашего проекта
    // Можно переопределить через RUST_LOG=debug или RUST_LOG=mmdnca=debug
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| {
            EnvFilter::new("warn,mmdnca=info")
        });

    // Создаем неблокирующий файловый appender (один файл без ротации)
    let file_appender = tracing_appender::rolling::never("./logs", "app.log");
    let (non_blocking_file, _log_guard) = tracing_appender::non_blocking(file_appender);
    // ВАЖНО: _log_guard должен жить до конца программы, иначе запись в файл остановится

    // Слой для вывода в консоль с цветами
    let console_layer = fmt::layer()
        .with_target(false)
        .with_thread_ids(false)
        .with_file(true)
        .with_line_number(true)
        .with_level(true)
        .with_ansi(true)
        .with_span_events(FmtSpan::NONE)
        .with_timer(timer.clone())
        .with_writer(std::io::stdout);

    // Слой для записи в файл (без цветов)
    let file_layer = fmt::layer()
        .with_target(false)
        .with_thread_ids(false)
        .with_file(true)
        .with_line_number(true)
        .with_level(true)
        .with_ansi(false)  // Без ANSI цветов в файле
        .with_span_events(FmtSpan::NONE)
        .with_timer(timer)
        .with_writer(non_blocking_file);

    // Объединяем слои и инициализируем
    tracing_subscriber::registry()
        .with(filter)
        .with(console_layer)
        .with(file_layer)
        .init();
    
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
        .signature_type(polymarket_client_sdk::clob::types::SignatureType::GnosisSafe) // EOA - обычная подпись кошелька
        .authenticate()
        .await?;

    tracing::info!("✅ Аутентификация CLOB успешна");

    let ws_client = WsClient::default().authenticate(credentials, funder_address)?;

    tracing::info!("✅ Аутентификация WsUser успешна");

    let scanner = AutoScanner::new();
    
    loop {
        println!("\n1. Найти цель и запустить ТОРГОВЛЮ | 2. Выход");
        print!("> "); io::stdout().flush().unwrap();
        
        let mut input = String::new();
        io::stdin().read_line(&mut input).unwrap();
        
        if input.trim() == "1" {
            // Ищем подходящий рынок
            if let Some(target) = scanner.find_next_target("btc-updown-15m", 0.0, 15.0).await {
                // Создаем реальный движок, передавая клиента и подписанта
                let engine = Arc::new(RealEngine::new(
                    client.clone(),
                    signer.clone(),
                    target.up_token.clone(),
                    target.down_token.clone()
                ));

                let market_stream = DataStream::new(target.up_token.clone(), target.down_token.clone(), engine.clone(), ws_market_url.clone());

                // ! ИЗМЕНЕНИЕ: Передаем ws_client (который уже аутентифицирован в начале main)
                let user_stream = UserStream::new(engine.clone(), ws_client.clone());

                tracing::info!("🏁 Запуск торговой сессии...");

                tokio::select! {
                    res = market_stream.start_stream(target.end_date) => {
                        if let Err(e) = res { tracing::error!("📉 Market stream died: {}", e); }
                    }
                    res = user_stream.start_stream() => {
                        if let Err(e) = res { tracing::error!("👤 User stream died: {}", e); }
                    }
                }
            }
        } else { break; }
    }
    Ok(())
}
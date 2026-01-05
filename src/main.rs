mod models; mod scanner; mod websocket; mod engine; mod report; mod websocket_user;
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
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

    println!("✅ Аутентификация CLOB успешна");

    let ws_client = WsClient::default().authenticate(credentials, funder_address)?;

    println!("✅ Аутентификация WsUser успешна");

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
                    &target.slug,
                    client.clone(),
                    signer.clone(),
                    target.up_token.clone(),
                    target.down_token.clone()
                ));



                let market_stream = DataStream::new(target.up_token.clone(), target.down_token.clone(), engine.clone(), ws_market_url.clone());

                // ! ИЗМЕНЕНИЕ: Передаем ws_client (который уже аутентифицирован в начале main)
                let user_stream = UserStream::new(engine.clone(), ws_client.clone());

                println!("🏁 Запуск торговой сессии...");

                tokio::select! {
                    res = market_stream.start_stream(target.end_date) => {
                        if let Err(e) = res { eprintln!("📉 Market stream died: {}", e); }
                    }
                    res = user_stream.start_stream() => {
                        if let Err(e) = res { eprintln!("👤 User stream died: {}", e); }
                    }
                }
            }
        } else { break; }
    }
    Ok(())
}
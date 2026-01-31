use alloy::primitives::Address;
use alloy::signers::Signer as _;
use alloy::signers::local::PrivateKeySigner;
use polymarket_client_sdk::auth::Credentials;
use polymarket_client_sdk::{POLYGON, PRIVATE_KEY_VAR};
use std::str::FromStr as _;
use uuid::Uuid;

/// Структура с переменными окружения
pub struct EnvConfig {
    pub api_key: Uuid,
    pub credentials: Credentials,
    pub signer: PrivateKeySigner,
    pub funder_address: Address,
    pub ws_market_url: String,
}

/// Загружает все необходимые переменные окружения
pub fn load_env_config() -> anyhow::Result<EnvConfig> {
    let api_key = Uuid::parse_str(&std::env::var("POLYMARKET_API_KEY")?)?;
    let api_secret = std::env::var("POLYMARKET_API_SECRET")?;
    let api_passphrase = std::env::var("POLYMARKET_API_PASSPHRASE")?;
    let private_key = std::env::var(PRIVATE_KEY_VAR).expect("Нужен PRIVATE_KEY_VAR в .env");
    let funder_addr_str = std::env::var("FUNDER_ADDRESS").expect("Нужен FUNDER_ADDRESS в .env");
    let funder_address: Address = funder_addr_str
        .parse()
        .expect("Неверный формат адреса в FUNDER_ADDRESS (должен начинаться с 0x...)");
    let ws_market_url = std::env::var("CLOB_WS_MARKET").expect("Нужен CLOB_WS_MARKET в .env");

    let signer = PrivateKeySigner::from_str(&private_key)?.with_chain_id(Some(POLYGON));
    let credentials = Credentials::new(api_key, api_secret, api_passphrase);

    Ok(EnvConfig {
        api_key,
        credentials,
        signer,
        funder_address,
        ws_market_url,
    })
}

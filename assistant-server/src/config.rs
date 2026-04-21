use config::{Config, Environment, File};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Settings {
    pub server: ServerConfig,
    pub wechat: WeChatConfig,
    pub security: SecurityConfig,
    pub cors: CorsConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    pub address: String,
    pub port: u16,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WeChatConfig {
    pub api_base_url: String,
    pub oauth_app_id: String,
    pub oauth_app_secret: String,
    pub redirect_uri: String,
    pub qr_bot_type: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SecurityConfig {
    pub session_ttl_hours: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CorsConfig {
    pub allowed_origins: Vec<String>,
    pub allow_credentials: bool,
}

impl Settings {
    pub fn load() -> Result<Self, config::ConfigError> {
        let app_env = std::env::var("APP_ENV").unwrap_or_else(|_| "development".to_string());
        let cfg = Config::builder()
            .add_source(File::with_name("config/development").required(false))
            .add_source(File::with_name(&format!("config/{app_env}")).required(false))
            .add_source(Environment::default().separator("__"))
            .build()?;
        cfg.try_deserialize()
    }
}

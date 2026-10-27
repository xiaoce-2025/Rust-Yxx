use config::{Config, Environment, File};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Settings {
    pub server: ServerConfig,
    pub wechat: WeChatConfig,
    pub security: SecurityConfig,
    pub cors: CorsConfig,
    #[serde(rename = "audioapi_paraformer")]
    pub paraformer: ParaformerConfig,
    #[serde(rename = "llm_deepseek")]
    pub deepseek: DeepSeekConfig,
    #[serde(default)]
    pub pkuvideo: PkuVideoConfig,
    #[serde(default)]
    pub video_translation: VideoTranslationConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct VideoTranslationConfig {
    pub enabled: bool,
    pub mimo_api_key: Option<String>,
    pub mimo_base_url: String,
    pub mimo_model: String,
    pub voice: String,
    pub max_speed: f64,
    pub asr_timeout_seconds: u64,
    pub refine_subtitles: bool,
    pub translation_concurrency: usize,
    pub tts_concurrency: usize,
    pub tts_requests_per_minute: usize,
}

impl Default for VideoTranslationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mimo_api_key: None,
            mimo_base_url: "https://api.xiaomimimo.com/v1".into(),
            mimo_model: "mimo-v2.5-tts".into(),
            voice: "苏打".into(),
            max_speed: 1.15,
            asr_timeout_seconds: 21600,
            refine_subtitles: true,
            translation_concurrency: 8,
            tts_concurrency: 8,
            tts_requests_per_minute: 100,
        }
    }
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

#[derive(Debug, Clone, Deserialize)]
pub struct ParaformerConfig {
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub upload_url: Option<String>,
    pub transcription_url: Option<String>,
    pub task_query_url: Option<String>,
    pub model: Option<String>,
    pub max_retries: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeepSeekConfig {
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PkuVideoConfig {
    #[serde(default = "default_m3u8_concurrency")]
    pub processing_concurrency: usize,
    #[serde(default = "default_m3u8_concurrency")]
    pub m3u8_concurrency: usize,
    #[serde(default = "default_m3u8_segment_timeout")]
    pub m3u8_segment_timeout_seconds: u64,
}

fn default_m3u8_concurrency() -> usize { 3 }
fn default_m3u8_segment_timeout() -> u64 { 120 }

impl Default for PkuVideoConfig {
    fn default() -> Self {
        PkuVideoConfig {
            processing_concurrency: 3,
            m3u8_concurrency: 3,
            m3u8_segment_timeout_seconds: 120,
        }
    }
}

impl Settings {
    pub fn load() -> Result<Self, config::ConfigError> {
        let app_env = std::env::var("APP_ENV").unwrap_or_else(|_| "development".to_string());
        let cfg = Config::builder()
            .add_source(File::with_name("config/development.toml").required(false))
            .add_source(File::with_name(&format!("config/{app_env}.toml")).required(false))
            .add_source(Environment::default().separator("__"))
            .build()?;
        cfg.try_deserialize()
    }
}

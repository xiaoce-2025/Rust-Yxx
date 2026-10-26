mod app_state;
mod auth;
mod config;
mod error;
mod models;
mod plugins;
mod routes;
mod services;
mod store;

use std::net::SocketAddr;
use std::sync::Arc;
use std::path::PathBuf;

use app_state::AppState;
use tracing::info;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let settings = config::Settings::load()?;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
                .add_directive("app_replay_download=info".parse()?)
                .add_directive("video_translation=info".parse()?),
        )
        .init();

    let data_dir = PathBuf::from("./data");
    let store = Arc::new(store::FileStore::new(data_dir).await?);

    let app_state = AppState::new(settings, store);
    let cache = app_state.login_cache.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        loop { interval.tick().await; cache.cleanup_expired().await; }
    });
    // 从FileStore加载pkuvideo_jobs到内存
    app_state.load_pkuvideo_jobs().await;
    app_state.load_pkuvideo_fw_jobs().await;
    if let Err(error) = services::video_translation::restore(&app_state).await {
        tracing::error!(%error, "视频翻译恢复失败，已有功能继续运行");
    }

    services::scheduler::start_wechat_listener_scheduler(app_state.clone());

    let app = routes::router(app_state.clone());

    let addr: SocketAddr = format!("{}:{}", app_state.settings.server.address, app_state.settings.server.port)
        .parse()?;
    info!("assistant-server listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

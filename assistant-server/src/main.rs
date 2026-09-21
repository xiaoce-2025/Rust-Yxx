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
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let data_dir = PathBuf::from("./data");
    let store = Arc::new(store::FileStore::new(data_dir).await?);

    let app_state = AppState::new(settings, store);
    // 从FileStore加载pkuvideo_jobs到内存
    app_state.load_pkuvideo_jobs().await;
    app_state.load_pkuvideo_fw_jobs().await;

    services::scheduler::start_wechat_listener_scheduler(app_state.clone());

    let app = routes::router(app_state.clone());

    let addr: SocketAddr = format!("{}:{}", app_state.settings.server.address, app_state.settings.server.port)
        .parse()?;
    info!("assistant-server listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

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
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let migrate_only = match std::env::args().skip(1).collect::<Vec<_>>().as_slice() {
        [] => false,
        [flag] if flag == "--migrate-db" => true,
        _ => return Err("用法: assistant-server [--migrate-db]".into()),
    };

    let settings = if migrate_only { None } else { Some(config::Settings::load()?) };

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
                .add_directive("app_replay_download=info".parse()?)
                .add_directive("teaching_ta=info".parse()?)
                .add_directive("video_translation=info".parse()?),
        )
        .init();

    let data_dir = PathBuf::from("./data");
    std::fs::create_dir_all(&data_dir)?;
    let instance_lock = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(data_dir.join("assistant.lock"))?;
    instance_lock.try_lock().map_err(|error| {
        std::io::Error::other(format!("无法锁定服务数据目录 {}: {error}", data_dir.display()))
    })?;

    let store = Arc::new(store::FileStore::new(data_dir.clone()).await?);
    let Some(settings) = settings else {
        store.import_legacy_section_files().await?;
        println!("{}", serde_json::to_string_pretty(&store.verify_database().await?)?);
        return Ok(());
    };
    let login_cache = Arc::new(services::login_cache::LoginCache::persistent(
        data_dir.join("teaching_sessions"),
    )?);

    let app_state = AppState::with_login_cache(settings, store, login_cache);
    let cache = app_state.login_cache.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        loop { interval.tick().await; cache.cleanup_missing_cookies().await; }
    });
    let session_store = app_state.store.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
        loop {
            interval.tick().await;
            if let Err(error) = session_store.prune_sessions().await {
                tracing::warn!(%error, "清理过期会话失败");
            }
        }
    });

    // Restore the in-process scheduler state.
    app_state.load_pkuvideo_jobs().await?;
    app_state.load_pkuvideo_fw_jobs().await?;
    let billing_state=app_state.clone();
    tokio::spawn(async move {
        let mut interval=tokio::time::interval(std::time::Duration::from_secs(60));
        loop { interval.tick().await; services::video_jobs::retry_pending_billing(&billing_state).await; }
    });
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

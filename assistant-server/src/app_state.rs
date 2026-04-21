use std::{collections::HashMap, sync::Arc};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::{
    config::Settings,
    plugins::router::PluginRouter,
    services::{pkuvideo::PkuVideoJob, wechat::WeChatService},
    store::FileStore,
};

#[derive(Clone)]
pub struct AppState {
    pub settings: Arc<Settings>,
    pub store: Arc<FileStore>,
    pub wechat_service: Arc<WeChatService>,
    pub plugin_router: Arc<PluginRouter>,
    pub in_memory_sessions: Arc<RwLock<HashMap<Uuid, RuntimeWechatSession>>>,
    pub pkuvideo_jobs: Arc<RwLock<HashMap<Uuid, PkuVideoJob>>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeWechatSession {
    pub user_id: Uuid,
    pub qrcode: String,
    pub qrcode_link: String,
    pub created_at: DateTime<Utc>,
}

impl AppState {
    pub fn new(settings: Settings, store: Arc<FileStore>) -> Self {
        let wechat_service = WeChatService::new(settings.wechat.clone());

        Self {
            settings: Arc::new(settings),
            store,
            wechat_service: Arc::new(wechat_service),
            plugin_router: Arc::new(PluginRouter::new()),
            in_memory_sessions: Arc::new(RwLock::new(HashMap::new())),
            pkuvideo_jobs: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

use std::{collections::HashMap, sync::Arc};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::{
    config::Settings,
    plugins::router::PluginRouter,
    services::{login_cache::LoginCache, pkuvideo::PkuVideoJob, pkuvideo_fw::PkuVideoFwJob, wechat::WeChatService},
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
    pub pkuvideo_fw_jobs: Arc<RwLock<HashMap<Uuid, PkuVideoFwJob>>>,
    pub fw_slots: Arc<tokio::sync::Mutex<HashMap<Uuid, Arc<tokio::sync::Mutex<()>>>>>,
    pub login_cache: Arc<LoginCache>,
    pub processing_users: Arc<RwLock<HashMap<Uuid, ProcessingState>>>,
    pub app_replay_clients: Arc<tokio::sync::Mutex<HashMap<Uuid, Arc<tokio::sync::Mutex<crate::services::app_replay::AppReplayClient>>>>>,
    pub lesson_mutations: Arc<tokio::sync::Mutex<HashMap<Uuid, Arc<tokio::sync::Mutex<()>>>>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeWechatSession {
    pub user_id: Uuid,
    pub qrcode: String,
    pub qrcode_link: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct ProcessingState {
    pub course_id: String,
    pub video_id: String,
    pub started_at: DateTime<Utc>,
}

impl AppState {
    pub async fn fw_slot(&self, user_id: Uuid) -> Arc<tokio::sync::Mutex<()>> {
        self.fw_slots.lock().await.entry(user_id).or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))).clone()
    }
    pub async fn lesson_mutation(&self, user_id: Uuid) -> Arc<tokio::sync::Mutex<()>> {
        self.lesson_mutations.lock().await.entry(user_id)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))).clone()
    }
    pub async fn app_replay_client(&self, user_id: Uuid) -> Result<Arc<tokio::sync::Mutex<crate::services::app_replay::AppReplayClient>>, String> {
        let mut clients = self.app_replay_clients.lock().await;
        if let Some(client) = clients.get(&user_id) { return Ok(client.clone()); }
        let client = Arc::new(tokio::sync::Mutex::new(crate::services::app_replay::AppReplayClient::for_user(user_id).await?));
        clients.insert(user_id, client.clone());
        Ok(client)
    }
    pub fn new(settings: Settings, store: Arc<FileStore>) -> Self {
        let wechat_service = WeChatService::new(settings.wechat.clone());

        let pkuvideo_jobs = Arc::new(RwLock::new(HashMap::new()));
        let pkuvideo_fw_jobs = Arc::new(RwLock::new(HashMap::new()));

        let login_cache = Arc::new(LoginCache::new(10)); // 10分钟缓存

        let processing_users = Arc::new(RwLock::new(HashMap::new()));

        Self {
            settings: Arc::new(settings),
            store,
            wechat_service: Arc::new(wechat_service),
            plugin_router: Arc::new(PluginRouter::new()),
            in_memory_sessions: Arc::new(RwLock::new(HashMap::new())),
            pkuvideo_jobs,
            pkuvideo_fw_jobs,
            fw_slots: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            login_cache,
            processing_users,
            app_replay_clients: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            lesson_mutations: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        }
    }

    pub async fn load_pkuvideo_jobs(&self) {
        let jobs = self.store.get_all_pkuvideo_jobs().await;
        let mut in_memory_jobs = self.pkuvideo_jobs.write().await;
        *in_memory_jobs = jobs;
    }

    pub async fn load_pkuvideo_fw_jobs(&self) {
        let jobs = self.store.get_all_pkuvideo_fw_jobs().await;
        let mut in_memory_jobs = self.pkuvideo_fw_jobs.write().await;
        *in_memory_jobs = jobs;
    }
}

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
    pub batch_submissions: Arc<crate::services::batch_submission::BatchSubmissionManager>,
    pub video_queue: Arc<crate::services::video_queue::VideoQueue>,
    pub settings: Arc<Settings>,
    pub store: Arc<FileStore>,
    pub wechat_service: Arc<WeChatService>,
    pub plugin_router: Arc<PluginRouter>,
    pub in_memory_sessions: Arc<RwLock<HashMap<Uuid, RuntimeWechatSession>>>,
    pub pkuvideo_jobs: Arc<RwLock<HashMap<Uuid, PkuVideoJob>>>,
    pub pkuvideo_fw_jobs: Arc<RwLock<HashMap<Uuid, PkuVideoFwJob>>>,
    pub video_translations: Arc<crate::services::video_translation::TranslationManager>,
    pub fw_slots: Arc<tokio::sync::Mutex<HashMap<Uuid, Arc<tokio::sync::Mutex<()>>>>>,
    pub login_cache: Arc<LoginCache>,
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


impl AppState {
    pub async fn course_busy(&self, user_id: Uuid, course: &str) -> bool {
        self.pkuvideo_jobs.read().await.values().any(|j|j.user_id == user_id
            && j.course_id.as_deref() == Some(course) && matches!(j.status, crate::services::pkuvideo::PipelineStatus::Pending | crate::services::pkuvideo::PipelineStatus::Running))
    }
    pub async fn lesson_busy(&self, user_id: Uuid, course: &str, lesson: &str) -> bool {
        self.pkuvideo_jobs.read().await.values().any(|j|j.user_id == user_id
            && j.course_id.as_deref() == Some(course) && j.lesson_id.as_deref() == Some(lesson)
            && matches!(j.status, crate::services::pkuvideo::PipelineStatus::Pending | crate::services::pkuvideo::PipelineStatus::Running))
    }
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

        let login_cache = Arc::new(LoginCache::new(60)); // 仅在操作开始时检查 1 小时有效期


        let state = Self {
            batch_submissions: Arc::new(crate::services::batch_submission::BatchSubmissionManager::new()),
            video_queue: crate::services::video_queue::VideoQueue::new(settings.pkuvideo.processing_concurrency),
            settings: Arc::new(settings),
            store,
            wechat_service: Arc::new(wechat_service),
            plugin_router: Arc::new(PluginRouter::new()),
            in_memory_sessions: Arc::new(RwLock::new(HashMap::new())),
            pkuvideo_jobs,
            pkuvideo_fw_jobs,
            video_translations: Arc::new(crate::services::video_translation::TranslationManager::new()),
            fw_slots: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            login_cache,
            app_replay_clients: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            lesson_mutations: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        };
        state.video_queue.start(Arc::new(crate::services::video_jobs::VideoTaskHandler { state: state.clone() }));
        state
    }

    pub async fn load_pkuvideo_jobs(&self) {
        let mut jobs = self.store.get_all_pkuvideo_jobs().await;
        for job in jobs.values_mut() {
            if matches!(job.status, crate::services::pkuvideo::PipelineStatus::Pending | crate::services::pkuvideo::PipelineStatus::Running) {
                job.status = crate::services::pkuvideo::PipelineStatus::Failed;
                job.postprocessing = false;
                job.error = Some("服务重启中断任务，请重新提交；已保存的课节密钥可直接复用".into());
                job.updated_at = Utc::now();
                let _ = self.store.update_pkuvideo_job(job.clone()).await;
            }
        }
        let mut in_memory_jobs = self.pkuvideo_jobs.write().await;
        *in_memory_jobs = jobs;
    }

    pub async fn load_pkuvideo_fw_jobs(&self) {
        let jobs = self.store.get_all_pkuvideo_fw_jobs().await;
        let mut in_memory_jobs = self.pkuvideo_fw_jobs.write().await;
        *in_memory_jobs = jobs;
    }
}

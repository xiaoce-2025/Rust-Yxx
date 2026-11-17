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
    pub ta_ai: Arc<crate::services::ta_ai::Manager>,
    pub ta_mutations: Arc<tokio::sync::Mutex<HashMap<Uuid, Arc<tokio::sync::Mutex<()>>>>>,
    pub sso_transactions: Arc<tokio::sync::Mutex<HashMap<String, crate::routes::sso::Transaction>>>,
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
    pub async fn ta_mutation(&self, user_id: Uuid) -> Arc<tokio::sync::Mutex<()>> {
        self.ta_mutations.lock().await.entry(user_id)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))).clone()
    }
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
        let client = Arc::new(tokio::sync::Mutex::new(crate::services::app_replay::AppReplayClient::for_user(self.store.clone(), user_id).await?));
        clients.insert(user_id, client.clone());
        Ok(client)
    }
    pub fn new(settings: Settings, store: Arc<FileStore>) -> Self {
        Self::with_login_cache(settings, store, Arc::new(LoginCache::new(60)))
    }
    pub fn with_login_cache(settings: Settings, store: Arc<FileStore>, login_cache: Arc<LoginCache>) -> Self {
        let wechat_service = WeChatService::new(settings.wechat.clone());

        let pkuvideo_jobs = Arc::new(RwLock::new(HashMap::new()));
        let pkuvideo_fw_jobs = Arc::new(RwLock::new(HashMap::new()));



        let state = Self {
            ta_ai: Arc::new(crate::services::ta_ai::Manager::new()),
            ta_mutations: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            sso_transactions: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
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
            if matches!(job.status, crate::services::pkuvideo::PipelineStatus::Pending | crate::services::pkuvideo::PipelineStatus::Running)
                && job.result_tex_sha256.is_none() {
                job.status = crate::services::pkuvideo::PipelineStatus::Failed;
                job.postprocessing = false;
                job.error = Some("服务重启中断任务，请重新提交；已有笔记保留".into());
                job.updated_at = Utc::now();
                job.finished_at = Some(Utc::now());
                if let Err(error) = self.store.update_pkuvideo_job(job.clone()).await {
                    tracing::error!(job_id=%job.id, %error, "持久化中断状态失败");
                }
            }
        }
        *self.pkuvideo_jobs.write().await = jobs.clone();
        // Exact private snapshots survive publication/installation failures. Retry only
        // delivery, never run AI again or silently declare an interrupted generation done.
        use crate::services::video_queue::TaskHandler;
        let handler = crate::services::video_jobs::VideoTaskHandler { state: self.clone() };
        let mut deliveries: Vec<_> = jobs.into_values().filter(|j|
            j.status != crate::services::pkuvideo::PipelineStatus::Succeeded && j.result_tex_sha256.is_some()
                && (j.postprocessing || matches!(j.status, crate::services::pkuvideo::PipelineStatus::Pending | crate::services::pkuvideo::PipelineStatus::Running))
        ).collect();
        deliveries.sort_by_key(|j|j.created_at);
        for mut job in deliveries {
            let superseded = self.pkuvideo_jobs.read().await.values().any(|newer|
                newer.user_id == job.user_id && newer.created_at > job.created_at
                && newer.status == crate::services::pkuvideo::PipelineStatus::Succeeded
                && match (&job.course_id, &job.lesson_id) {
                    (Some(course),Some(lesson)) => newer.course_id.as_ref()==Some(course) && newer.lesson_id.as_ref()==Some(lesson),
                    _ => newer.course_id.is_none() && newer.url_key==job.url_key,
                });
            if superseded {
                job.status = crate::services::pkuvideo::PipelineStatus::Failed;
                job.postprocessing = false;
                job.error = Some("后续任务已交付新版笔记，取消旧结果恢复".into());
                job.finished_at = Some(Utc::now());
                if let Err(error) = self.store.update_pkuvideo_job(job.clone()).await {
                    tracing::error!(job_id=%job.id,%error,"保存旧结果恢复取消状态失败");
                }
                self.pkuvideo_jobs.write().await.insert(job.id,job);
                continue;
            }
            let target = match (&job.course_id, &job.lesson_id) {
                (Some(course), Some(lesson)) => {
                    let Some(user) = self.store.get_user(job.user_id).await else { continue; };
                    crate::services::video_queue::CompletionTarget::CourseLesson {
                        username:user.email.split('@').next().unwrap_or_default().into(),
                        course_id:course.clone(), lesson_id:lesson.clone(),
                    }
                }
                _ => crate::services::video_queue::CompletionTarget::None,
            };
            let bytes = tokio::fs::read(self.store.user_tex_path(job.user_id, job.id)).await;
            let bytes = match bytes {
                Ok(bytes) if Some(crate::store::tex_hash(&bytes)) == job.result_tex_sha256 => bytes,
                _ => {
                handler.failed(job.id, "待安装的独立 TeX 副本校验失败，请重新提交".into()).await;
                continue;
                }
            };
            // A crash may precede the publication journal itself. Finish publishing
            // this compiled snapshot before delivery, unless a later task has
            // already advanced the global cache while this task was failed.
            if job.share_cache && job.cache_revision_used.is_none() {
                let Some(key) = job.url_key.as_deref() else {
                    handler.failed(job.id, "待恢复结果缺少缓存身份".into()).await;
                    continue;
                };
                let recovered = async {
                    let current = self.store.verified_tex(key).await?;
                    if let Some((entry, _)) = current {
                        if Some(entry.tex_sha256.as_str()) == job.result_tex_sha256.as_deref() {
                            return Ok(Some(entry.revision));
                        }
                        let newer = chrono::DateTime::parse_from_rfc3339(&entry.updated_at)
                            .is_ok_and(|time|time > job.updated_at);
                        if newer { return Ok(None); }
                    }
                    self.store.publish_tex(key, &bytes, job.id).await.map(|entry|Some(entry.revision))
                }.await;
                match recovered {
                    Ok(revision) => {
                        job.cache_revision_used = revision;
                        if let Err(error) = self.store.update_pkuvideo_job(job.clone()).await {
                            handler.failed(job.id, error.to_string()).await;
                            continue;
                        }
                        self.pkuvideo_jobs.write().await.insert(job.id, job.clone());
                    }
                    Err(error) => { handler.failed(job.id, format!("恢复缓存发布失败: {error}")).await; continue; }
                }
            }
            if let Err(error) = handler.completed(crate::services::video_queue::CompletionEvent {
                task_id:job.id, user_id:job.user_id, target, result:Ok(()),
            }).await { handler.failed(job.id,error).await; }
        }
    }

    pub async fn load_pkuvideo_fw_jobs(&self) {
        let jobs = self.store.get_all_pkuvideo_fw_jobs().await;
        let mut in_memory_jobs = self.pkuvideo_fw_jobs.write().await;
        *in_memory_jobs = jobs;
    }
}

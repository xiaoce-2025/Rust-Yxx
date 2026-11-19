//! Submission, processing and completion dispatch for explicit video tasks.
use super::{
    pkuvideo::{run_pipeline, PipelineStatus},
    prepared_media::PreparedMedia,
    video_queue::{CompletionEvent, CompletionTarget, TaskHandler, VideoTask},
};
use crate::{app_state::AppState, config::Settings};
use async_trait::async_trait;
use chrono::Utc;
use std::path::PathBuf;
use uuid::Uuid;

pub struct VideoTaskHandler {
    pub state: AppState,
}

#[async_trait]
impl TaskHandler for VideoTaskHandler {
    async fn process(&self, task: VideoTask) -> Result<(), String> {
        // Keep the prepared media in a private DB sidecar for interrupted admission
        // and missing-duration recovery. It is never included in public task JSON.
        let path=PathBuf::from("note_billing_media").join(task.id.to_string());
        self.state.store.save_sidecar(&path,&serde_json::to_string(&task.media).map_err(|e|e.to_string())?).await
            .map_err(|e|format!("BILLING_SYSTEM_UNAVAILABLE: {e}"))?;
        match self.state.store.check_note_start(task.id,task.user_id).await {
            Ok(true) => {},
            Ok(false) => return Err(format!("{}: {}",crate::store::note_billing::INSUFFICIENT,crate::store::note_billing::INSUFFICIENT_MESSAGE)),
            Err(error) => return Err(format!("BILLING_SYSTEM_UNAVAILABLE: {error}")),
        }
        run_prepared_job(self.state.clone(), task.id, task.user_id, task.media).await
    }
    async fn completed(&self, event: CompletionEvent) -> Result<(), String> {
        event.result?;
        let user_id = event.user_id;
        let mut snapshot=self.state.pkuvideo_jobs.read().await.get(&event.task_id).cloned().ok_or("任务不存在")?;
        if snapshot.user_id != user_id { return Err("任务完成通知用户不匹配".into()); }
        if snapshot.status == PipelineStatus::Succeeded { return Ok(()); }
        // Legacy interrupted deliveries predate billing and must never be charged retroactively.
        let billable=match self.state.store.note_billing_state(event.task_id).await {
            Ok(state)=>state.is_some(),
            Err(error)=> {
                tracing::error!(task_id=%snapshot.id,%error,"读取结算状态失败");
                return billing_pending(&self.state,snapshot,"BILLING_COMMIT_FAILED").await;
            }
        };
        if billable {
            snapshot.billing_duration_seconds=snapshot.billing_duration_seconds.filter(|s|*s>0);
            if snapshot.billing_duration_seconds.is_none() {
                snapshot.billing_duration_seconds=resolve_duration(&self.state,&snapshot).await;
            }
            if snapshot.billing_duration_seconds.is_none() {
                return billing_pending(&self.state,snapshot,"DURATION_UNAVAILABLE").await;
            }
            // A private delivery must be readable before any installation or charge.
            let path=self.state.store.user_tex_path(user_id,event.task_id);
            let bytes=tokio::fs::read(path).await.map_err(|e|format!("交付副本不可读: {e}"))?;
            if Some(crate::store::tex_hash(&bytes))!=snapshot.result_tex_sha256 { return Err("交付副本校验失败".into()); }
            if let Err(error)=self.state.store.update_pkuvideo_job(snapshot.clone()).await {
                tracing::error!(task_id=%snapshot.id,%error,"保存交付计费信息失败");
                return billing_pending(&self.state,snapshot,"BILLING_COMMIT_FAILED").await;
            }
            self.state.pkuvideo_jobs.write().await.insert(snapshot.id,snapshot.clone());
        }
        match event.target {
            CompletionTarget::None => {
                let job = self
                    .state
                    .pkuvideo_jobs
                    .read()
                    .await
                    .get(&event.task_id)
                    .cloned()
                    .ok_or("任务不存在")?;
                if let (Some(key), Some(hash)) = (&job.url_key, &job.result_tex_sha256) {
                    if let Err(error)=self.state.store.set_standalone_initial_hash(user_id,key,hash).await {
                        if billable {return billing_pending(&self.state,snapshot,"BILLING_COMMIT_FAILED").await;}
                        return Err(error);
                    }
                }
            }
            CompletionTarget::CourseLesson {
                username,
                course_id,
                lesson_id,
            } => {
                if snapshot.course_notes_path.is_none() {
                    if let Err(error)=super::course_processing::complete_lesson(&self.state,event.task_id,event.user_id,&username,&course_id,&lesson_id).await {
                        if billable && error.starts_with("BILLING_DELIVERY_COMMIT_FAILED") {
                            let current=self.state.pkuvideo_jobs.read().await.get(&event.task_id).cloned().ok_or("任务不存在")?;
                            return billing_pending(&self.state,current,"BILLING_COMMIT_FAILED").await;
                        }
                        return Err(format!("课节后处理失败: {error}"));
                    }
                }
            }
        }
        let mut job = self.state.pkuvideo_jobs.read().await.get(&event.task_id).cloned().ok_or("任务不存在")?;
        if billable {
            match self.state.store.charge_note_job(job.clone(),snapshot.billing_duration_seconds.unwrap()).await {
                Ok(charged) => job=charged,
                Err(error) => {
                    tracing::error!(task_id=%job.id,%error,"结算待恢复");
                    return billing_pending(&self.state,job,"BILLING_COMMIT_FAILED").await;
                }
            }
        } else {
            job.status=PipelineStatus::Succeeded; job.error=None;job.postprocessing=false;
            job.updated_at=Utc::now();job.finished_at=Some(Utc::now());
            self.state.store.update_pkuvideo_job(job.clone()).await.map_err(|e|e.to_string())?;
        }
        let _=self.state.store.delete_sidecar(&PathBuf::from("note_billing_media").join(job.id.to_string())).await;
        let artifacts=job.artifacts_dir.clone();
        let installed_course=job.course_id.is_some();
        self.state.pkuvideo_jobs.write().await.insert(job.id,job);
        if installed_course {
            // The course copy is now committed; the delivery snapshot is no longer needed.
            let _ = tokio::fs::remove_file(self.state.store.user_tex_path(user_id, event.task_id)).await;
        }
        if let Err(error) = tokio::fs::remove_dir_all(&artifacts).await {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(task_id=%event.task_id, %error, "清理任务工作区失败");
            }
        }
        Ok(())
    }
    async fn failed(&self, task_id: Uuid, error: String) {
        if self.state.pkuvideo_jobs.read().await.get(&task_id).is_some_and(|j|j.billing_state.as_deref()==Some("BILLING_PENDING")) { return; }
        if error.starts_with("BILLING_SYSTEM_UNAVAILABLE") {
            let mut jobs=self.state.pkuvideo_jobs.write().await;
            if let Some(job)=jobs.get_mut(&task_id) {
                job.status=PipelineStatus::Pending;job.billing_state=Some("QUEUED".into());
                job.failure_code=Some("BILLING_SYSTEM_UNAVAILABLE".into());job.error=Some("计费服务暂时不可用，等待重试".into());
                let _=self.state.store.update_pkuvideo_job(job.clone()).await;
            }
            return;
        }
        let billable=match self.state.store.note_billing_state(task_id).await {
            Ok(Some(state)) if state=="CHARGED" || state=="BILLING_PENDING" => return,
            Err(error) => { tracing::error!(%task_id,%error,"无法确认结算状态，保留任务等待恢复"); return; }
            Ok(state)=>state.is_some(),
        };
        let insufficient=error.starts_with(crate::store::note_billing::INSUFFICIENT);
        let _=self.state.store.set_note_billing_state(task_id,"FAILED_NO_CHARGE",Some(if insufficient {crate::store::note_billing::INSUFFICIENT} else {"PROCESSING_FAILED"})).await;
        let mut jobs = self.state.pkuvideo_jobs.write().await;
        if let Some(job) = jobs.get_mut(&task_id) {
            for step in job
                .steps
                .iter_mut()
                .filter(|step| step.status == PipelineStatus::Running)
            {
                step.status = PipelineStatus::Failed;
                step.message = error.clone();
                step.finished_at = Some(Utc::now());
            }
            if job.status == PipelineStatus::Succeeded { return; }
            job.billing_state=Some(if insufficient {"REJECTED_INSUFFICIENT"} else {"FAILED_NO_CHARGE"}.into());
            job.failure_code=Some(if insufficient {crate::store::note_billing::INSUFFICIENT} else {"PROCESSING_FAILED"}.into());
            job.status = PipelineStatus::Failed;
            job.postprocessing = job.result_tex_sha256.is_some() && !billable;
            job.error = Some(error);
            job.updated_at = Utc::now();
            job.finished_at = Some(Utc::now());
            if let Err(error) = self.state.store.update_pkuvideo_job(job.clone()).await {
                tracing::error!(%task_id,error=%error,"保存任务失败状态失败");
            }
            if let Some(saved)=self.state.store.get_pkuvideo_job(task_id).await {
                if saved.status==PipelineStatus::Succeeded {*job=saved;}
            }
        }
    }
}

/// Persist metadata and submit plain data. Returns once accepted, never waits for processing.
pub async fn enqueue_prepared_job(
    state: &AppState,
    job_id: Uuid,
    user_id: Uuid,
    media: PreparedMedia,
) -> Result<Uuid, String> {
    let user = state.store.get_user(user_id).await.ok_or("用户不存在")?;
    let priority = user.processing_priority;
    if state.pkuvideo_jobs.read().await.get(&job_id).is_none_or(|job|job.user_id!=user_id) {
        return Err("任务不存在或无权提交".into());
    }
    state.store.queue_note_billing(job_id,user_id).await.map_err(|e|e.to_string())?;
    state.store.save_sidecar(&PathBuf::from("note_billing_media").join(job_id.to_string()),&serde_json::to_string(&media).map_err(|e|e.to_string())?).await?;
    let task = {
        let mut jobs = state.pkuvideo_jobs.write().await;
        let job = jobs.get_mut(&job_id).ok_or("任务不存在")?;
        if job.user_id != user_id {
            return Err("无权提交此任务".into());
        }
        job.priority = priority;
        job.billing_state=Some("QUEUED".into());
        job.video_url = media.url.clone();
        job.url_key = Some(crate::store::tex_hash(media.url.as_bytes()));
        if job.share_cache {
            state
                .store
                .observe_tex_course(job)
                .await
                .map_err(|e| e.to_string())?;
        }
        let completion = match (&job.course_id, &job.lesson_id) {
            (Some(course_id), Some(lesson_id)) => CompletionTarget::CourseLesson {
                username: user.email.split('@').next().unwrap_or_default().into(),
                course_id: course_id.clone(),
                lesson_id: lesson_id.clone(),
            },
            _ => CompletionTarget::None,
        };
        state
            .store
            .update_pkuvideo_job(job.clone())
            .await
            .map_err(|e| e.to_string())?;
        VideoTask {
            id: job_id,
            user_id,
            priority,
            submitted_at: job.created_at,
            media,
            completion,
        }
    };
    state.video_queue.submit(task).await
}

async fn run_prepared_job(
    state: AppState,
    job_id: Uuid,
    user_id: Uuid,
    media: crate::services::prepared_media::PreparedMedia,
) -> Result<(), String> {
    run_prepared_job_using(
        state,
        job_id,
        user_id,
        media,
        |dir, settings, media| async move {
            run_pipeline(dir, PathBuf::from("./data/pkuvideo_jobs"), &settings, media).await
        },
    )
    .await
}

type PipelineOutput = Result<(Option<String>, Option<String>, Option<String>), String>;

async fn run_prepared_job_using<G, F>(
    state: AppState,
    job_id: Uuid,
    user_id: Uuid,
    media: PreparedMedia,
    generate: G,
) -> Result<(), String>
where
    G: FnOnce(PathBuf, Settings, PreparedMedia) -> F,
    F: std::future::Future<Output = PipelineOutput>,
{
    mark_job_running(&state, job_id).await;
    let job = state
        .pkuvideo_jobs
        .read()
        .await
        .get(&job_id)
        .cloned()
        .ok_or("任务不存在")?;
    let key = crate::store::tex_hash(media.url.as_bytes());
    let initial = if let (Some(course), Some(lesson)) = (&job.course_id, &job.lesson_id) {
        let user = state.store.get_user(user_id).await.ok_or("用户不存在")?;
        let username = user.email.split('@').next().unwrap_or_default();
        super::lesson_storage::load_lessons(&state.store, username, course)
            .await?
            .into_iter()
            .find(|l| &l.id == lesson)
            .and_then(|l| l.initial_tex_hash)
    } else {
        state.store.standalone_initial_hash(user_id, &key).await?
    };
    let cache = if job.share_cache {
        state
            .store
            .verified_tex(&key)
            .await
            .map_err(|e| e.to_string())?
    } else {
        None
    };
    {
        let mut jobs = state.pkuvideo_jobs.write().await;
        let current = jobs.get_mut(&job_id).ok_or("任务不存在")?;
        current.regenerate_decision_initial_hash = initial.clone();
        current.regenerate_decision_global_hash = cache.as_ref().map(|(e, _)| e.tex_sha256.clone());
        state
            .store
            .update_pkuvideo_job(current.clone())
            .await
            .map_err(|e| e.to_string())?;
    }
    if crate::store::reuse_cached_tex(
        job.regenerate,
        initial.as_deref(),
        cache.as_ref().map(|(e, _)| e.tex_sha256.as_str()),
    ) {
        let (entry, bytes) = cache.unwrap();
        let seconds=match entry.video_duration_seconds.filter(|s|*s>0) {
            Some(seconds)=>Some(seconds),
            None=> {
                let path=PathBuf::from(&job.artifacts_dir).join("duration_probe.mp4");
                let _=tokio::fs::create_dir_all(&job.artifacts_dir).await;
                match super::pkuvideo::download_prepared(&media,&path,state.settings.pkuvideo.m3u8_concurrency,state.settings.pkuvideo.m3u8_segment_timeout_seconds,None).await {
                    Ok(())=>probe_duration(&path).await,
                    Err(error)=>{ tracing::warn!(%job_id,%error,"历史缓存时长补录失败");None }
                }
            }
        };
        save_duration(&state,job_id,seconds).await?;
        if let Some(seconds)=seconds { state.store.set_tex_duration(&key,seconds).await.map_err(|e|e.to_string())?; }
        persist_delivery(&state, job_id, &bytes, Some(&entry), "cache_hit").await?;
        state
            .store
            .record_tex_hit(&key)
            .await
            .map_err(|e| e.to_string())?;
        tracing::info!(%job_id, url_key=%key, "使用全局 TeX 缓存，跳过生成流水线");
        return Ok(());
    }
    tracing::info!(%job_id, share_cache=job.share_cache, regenerate=job.regenerate, "运行完整笔记生成流水线");
    let user_config = state.store.get_config(user_id).await;
    let merged_settings = merge_with_user_config(&state.settings, &user_config);
    mark_step_running(&state, job_id, 0, "开始下载视频").await;
    let job_dir = state
        .pkuvideo_jobs
        .read()
        .await
        .get(&job_id)
        .map(|job| PathBuf::from(&job.artifacts_dir))
        .ok_or("任务不存在")?;
    let result = generate(job_dir.clone(), merged_settings, media).await;

    match result {
        Ok((transcript_path, notes_path, pdf_path)) => {
            let seconds=probe_duration(&job_dir.join("video.mp4")).await;
            save_duration(&state,job_id,seconds).await?;
            mark_step_success(&state, job_id, 0, "视频下载完成").await;
            mark_step_success(&state, job_id, 1, "mp3 抽离完成").await;
            mark_step_success(&state, job_id, 2, "语音转文字完成").await;
            mark_step_success(&state, job_id, 3, "讲义提取完成").await;
            mark_step_success(&state, job_id, 4, "PDF 编译完成").await;

            let _ = (transcript_path, pdf_path);
            let notes = notes_path.ok_or("流水线未生成 TeX")?;
            let bytes = tokio::fs::read(PathBuf::from("./data/pkuvideo_jobs").join(&notes))
                .await
                .map_err(|e| e.to_string())?;
            let source = if job.regenerate {
                "regenerated"
            } else {
                "generated"
            };
            // Save the compiled delivery first, so a crash after cache publication
            // can never lose the user's exact result.
            persist_delivery(&state, job_id, &bytes, None, source).await?;
            let published = if job.share_cache {
                Some(
                    state
                        .store
                        .publish_tex(&key, &bytes, job_id)
                        .await
                        .map_err(|e| e.to_string())?,
                )
            } else {
                None
            };
            if let (Some(entry),Some(seconds))=(&published,seconds) {
                state.store.set_tex_duration(&entry.url_key,seconds).await.map_err(|e|e.to_string())?;
            }
            persist_delivery(&state, job_id, &bytes, published.as_ref(), source).await?;
            Ok(())
        }
        Err(err) => {
            tracing::error!(%job_id, error=%err, "course processing failed");
            let (index, message) = parse_step_error(&err);
            mark_step_failed(&state, job_id, index, &message).await;
            Err(message)
        }
    }
}

// Persist a private snapshot before installation. Restart recovery can retry completion
// from these exact bytes even if another task has since published a newer global version.
async fn persist_delivery(
    state: &AppState,
    id: Uuid,
    bytes: &[u8],
    entry: Option<&crate::store::GlobalTexEntry>,
    source: &str,
) -> Result<(), String> {
    if !crate::store::valid_tex(bytes) {
        return Err("生成的 TeX 内容无效".into());
    }
    let mut jobs = state.pkuvideo_jobs.write().await;
    let job = jobs.get_mut(&id).ok_or("任务不存在")?;
    let destination = state.store.user_tex_path(job.user_id, id);
    crate::store::atomic_tex(&destination, bytes)?;
    let destination = destination.canonicalize().map_err(|e| e.to_string())?;
    job.notes_path = Some(destination.to_string_lossy().into());
    job.result_source = Some(source.into());
    job.result_tex_sha256 = Some(crate::store::tex_hash(bytes));
    job.cache_revision_used = entry.map(|e| e.revision);
    job.postprocessing = true;
    job.transcript_path = None;
    job.pdf_path = None;
    if source == "cache_hit" {
        job.steps = vec![super::pkuvideo::PipelineStep {
            name: "copying_cached_tex".into(),
            status: PipelineStatus::Succeeded,
            message: "使用缓存，跳过下载、转写和 AI 生成".into(),
            started_at: job.started_at,
            finished_at: Some(Utc::now()),
        }];
    }
    job.updated_at = Utc::now();
    state
        .store
        .update_pkuvideo_job(job.clone())
        .await
        .map_err(|e| e.to_string())
}

fn parse_step_error(raw: &str) -> (usize, String) {
    if let Some((step_part, message)) = raw.split_once(';') {
        if let Some(step) = step_part.strip_prefix("step=") {
            if let Ok(index) = step.parse::<usize>() {
                return (index.min(4), message.to_string());
            }
        }
    }
    (0, raw.to_string())
}

/// 合并全局配置和用户自定义配置，用户配置优先
pub(crate) fn provider_key(
    mode: Option<crate::store::AiMode>,
    environment: &str,
    personal: Option<String>,
    global: Option<String>,
) -> Option<String> {
    use crate::store::AiMode;
    let configured = |value: &String| !value.trim().is_empty();
    let personal = personal.filter(configured);
    let server = std::env::var(environment).ok().filter(configured);
    let global = global.filter(configured);
    match mode {
        Some(AiMode::Custom) => personal.or(server).or(global),
        Some(AiMode::Server) => server.or(global),
        None => server.or(personal).or(global),
    }
}

pub(crate) fn merge_with_user_config(
    global_settings: &Settings,
    user_config: &Option<crate::store::FileUserConfig>,
) -> Settings {
    use crate::store::AiMode;
    let mut merged = global_settings.clone();
    let mode = user_config.as_ref().and_then(|cfg| cfg.ai_mode);
    let overrides = user_config
        .as_ref()
        .filter(|_| mode != Some(AiMode::Server));
    if let Some(cfg) = overrides {
        if let Some(model) = cfg
            .aliyun_model
            .as_ref()
            .filter(|model| !model.trim().is_empty())
        {
            merged.paraformer.model = Some(model.clone());
        }
        if let Some(model) = cfg
            .deepseek_model
            .as_ref()
            .filter(|model| !model.trim().is_empty())
        {
            merged.deepseek.model = Some(model.clone());
        }
    }
    merged.paraformer.api_key = provider_key(
        mode,
        "DASHSCOPE_API_KEY",
        overrides.and_then(|cfg| cfg.aliyun_api_key.clone()),
        merged.paraformer.api_key,
    );
    merged.deepseek.api_key = provider_key(
        mode,
        "DEEPSEEK_API_KEY",
        overrides.and_then(|cfg| cfg.deepseek_api_key.clone()),
        merged.deepseek.api_key,
    );
    merged
}

async fn mark_job_running(state: &AppState, job_id: Uuid) {
    let billing=state.store.note_billing_state(job_id).await.ok().flatten();
    let mut jobs = state.pkuvideo_jobs.write().await;
    if let Some(job) = jobs.get_mut(&job_id) {
        job.billing_state=billing;
        job.failure_code=None;job.error=None;
        job.status = PipelineStatus::Running;
        job.started_at = Some(Utc::now());
        job.updated_at = Utc::now();
        // 保存到FileStore
        let _ = state.store.update_pkuvideo_job(job.clone()).await;
    }
}

async fn mark_step_running(state: &AppState, job_id: Uuid, index: usize, msg: &str) {
    let mut jobs = state.pkuvideo_jobs.write().await;
    if let Some(job) = jobs.get_mut(&job_id) {
        if let Some(step) = job.steps.get_mut(index) {
            step.status = PipelineStatus::Running;
            step.message = msg.to_string();
            step.started_at = Some(Utc::now());
        }
        job.updated_at = Utc::now();
        // 保存到FileStore
        let _ = state.store.update_pkuvideo_job(job.clone()).await;
    }
}

async fn mark_step_success(state: &AppState, job_id: Uuid, index: usize, msg: &str) {
    let mut jobs = state.pkuvideo_jobs.write().await;
    if let Some(job) = jobs.get_mut(&job_id) {
        if let Some(step) = job.steps.get_mut(index) {
            step.status = PipelineStatus::Succeeded;
            step.message = msg.to_string();
            step.finished_at = Some(Utc::now());
            if step.started_at.is_none() {
                step.started_at = step.finished_at;
            }
        }
        job.updated_at = Utc::now();
        // 保存到FileStore
        let _ = state.store.update_pkuvideo_job(job.clone()).await;
    }
}

async fn mark_step_failed(state: &AppState, job_id: Uuid, index: usize, msg: &str) {
    let mut jobs = state.pkuvideo_jobs.write().await;
    if let Some(job) = jobs.get_mut(&job_id) {
        if let Some(step) = job.steps.get_mut(index) {
            step.status = PipelineStatus::Failed;
            step.message = msg.to_string();
            step.finished_at = Some(Utc::now());
            if step.started_at.is_none() {
                step.started_at = step.finished_at;
            }
        }
        job.status = PipelineStatus::Failed;
        job.postprocessing = false;
        job.updated_at = Utc::now();
        job.error = Some(msg.to_string());
        // 保存到FileStore
        let _ = state.store.update_pkuvideo_job(job.clone()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{services::lesson_storage, store::FileStore};
    use serde_json::json;
    use std::sync::Arc;

    struct CourseFixture(PathBuf);
    impl Drop for CourseFixture {
        fn drop(&mut self) {
            if let (Ok(root), Ok(target)) = (
                std::fs::canonicalize("./course_data"),
                self.0.canonicalize(),
            ) {
                if target.starts_with(root)
                    && target
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("queue_completion_test_")
                {
                    let _ = std::fs::remove_dir_all(target);
                }
            }
        }
    }

    async fn state(dir: &std::path::Path) -> AppState {
        let settings: Settings = serde_json::from_value(json!({
            "server":{"address":"127.0.0.1","port":0},
            "wechat":{"api_base_url":"","oauth_app_id":"","oauth_app_secret":"","redirect_uri":"","qr_bot_type":""},
            "security":{"session_ttl_hours":1},"cors":{"allowed_origins":[],"allow_credentials":false},
            "audioapi_paraformer":{},"llm_deepseek":{}
        })).unwrap();
        AppState::new(
            settings,
            Arc::new(FileStore::new(dir.into()).await.unwrap()),
        )
    }

    async fn cache_user(state: &AppState) -> (Uuid, String, CourseFixture) {
        let id = Uuid::new_v4();
        let username = format!("queue_completion_test_{}", id.simple());
        state
            .store
            .insert_user(crate::store::FileUser {
                id,
                username: username.clone(),
                email: format!("{username}@example.invalid"),
                password_hash: String::new(),
                is_active: true,
                created_at: Utc::now(),
                updated_at: Utc::now(),
                processing_priority: 5,
                storage_quota: 10,
                processing_quota: 10,
                iaaa_encrypted_password: None,
                iaaa_public_key: None,
            })
            .await
            .unwrap();
        let lessons: Vec<lesson_storage::LessonInfo> = serde_json::from_value(json!([{"id":"lesson","video_url":"","play_url":"","date":"2026-11-17","time":"08:00","day_of_week":"","is_processed":false,"tex_filename":null}])).unwrap();
        lesson_storage::save_lessons(&state.store, &username, "course", &lessons)
            .await
            .unwrap();
        let cleanup = CourseFixture(PathBuf::from("./course_data").join(&username));
        (id, username, cleanup)
    }

    async fn cache_job(state: &AppState, user: Uuid, regenerate: bool) -> Uuid {
        let id = Uuid::new_v4();
        let mut job = super::super::pkuvideo::PkuVideoJob::new(
            id,
            user,
            "https://classroom.invalid/video?identity=exact".into(),
            state
                .store
                .user_tex_path(user, id)
                .parent()
                .unwrap()
                .join(id.to_string())
                .to_string_lossy()
                .into(),
        );
        job.regenerate = regenerate;
        job.share_cache = true;
        job.course_id = Some("course".into());
        job.lesson_id = Some("lesson".into());
        job.course_name = Some(format!("course-{user}"));
        job.course_date = Some("2026-11-17".into());
        job.url_key = Some(crate::store::tex_hash(job.video_url.as_bytes()));
        state.store.observe_tex_course(&job).await.unwrap();
        state.store.insert_pkuvideo_job(job.clone()).await.unwrap();
        state.pkuvideo_jobs.write().await.insert(id, job);
        id
    }

    async fn execute_fixture(
        state: &AppState,
        user: Uuid,
        username: &str,
        regenerate: bool,
        generated: Option<&str>,
    ) -> (Uuid, bool) {
        let id = cache_job(state, user, regenerate).await;
        let job = state.pkuvideo_jobs.read().await[&id].clone();
        let media = PreparedMedia {
            url: job.video_url,
            enc_key: None,
        };
        let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let calls = called.clone();
        let result = run_prepared_job_using(
            state.clone(),
            id,
            user,
            media,
            move |dir, _, _| async move {
                calls.store(true, std::sync::atomic::Ordering::SeqCst);
                let content = generated.ok_or("simulated generation failure")?;
                std::fs::create_dir_all(&dir).unwrap();
                let notes = dir.join("notes.tex");
                std::fs::write(&notes, content).unwrap();
                Ok((
                    None,
                    Some(notes.canonicalize().unwrap().to_string_lossy().into()),
                    None,
                ))
            },
        )
        .await;
        let handler = VideoTaskHandler {
            state: state.clone(),
        };
        match result {
            Ok(()) => handler
                .completed(CompletionEvent {
                    task_id: id,
                    user_id: user,
                    result: Ok(()),
                    target: CompletionTarget::CourseLesson {
                        username: username.into(),
                        course_id: "course".into(),
                        lesson_id: "lesson".into(),
                    },
                })
                .await
                .unwrap(),
            Err(error) => handler.failed(id, error).await,
        }
        (id, called.load(std::sync::atomic::Ordering::SeqCst))
    }

    async fn installed(state: &AppState, username: &str) -> (String, Option<String>, PathBuf) {
        let lessons = lesson_storage::load_lessons(&state.store, username, "course")
            .await
            .unwrap();
        let lesson = &lessons[0];
        let path = PathBuf::from("./course_data")
            .join(username)
            .join("course")
            .join(lesson.tex_filename.as_ref().unwrap());
        (
            std::fs::read_to_string(&path).unwrap(),
            lesson.initial_tex_hash.clone(),
            path,
        )
    }

    #[tokio::test]
    async fn worker_cache_and_regeneration_charge_after_delivery_once() {
        let temp=tempdir::TempDir::new("billing-worker-cache").unwrap();
        let state=state(temp.path()).await;
        let (user,name,_cleanup)=cache_user(&state).await;
        let key=crate::store::tex_hash(b"https://classroom.invalid/video?identity=exact");
        state.store.publish_tex(&key,b"\\section{Billing cached}",Uuid::new_v4()).await.unwrap();
        state.store.set_tex_duration(&key,7500).await.unwrap();
        for (regenerate,expected_balance) in [(false,8),(true,6)] {
            if regenerate {
                state.store.publish_tex(&key,b"\\section{New cached version}",Uuid::new_v4()).await.unwrap();
            }
            let id=cache_job(&state,user,regenerate).await;
            let handler=VideoTaskHandler{state:state.clone()};
            let target=CompletionTarget::CourseLesson{username:name.clone(),course_id:"course".into(),lesson_id:"lesson".into()};
            handler.process(VideoTask{id,user_id:user,priority:5,submitted_at:Utc::now(),media:PreparedMedia{url:"https://classroom.invalid/video?identity=exact".into(),enc_key:None},completion:target.clone()}).await.unwrap();
            assert_eq!(state.store.note_balance(user).await.unwrap(),expected_balance+2);
            for _ in 0..3 {handler.completed(CompletionEvent{task_id:id,user_id:user,target:target.clone(),result:Ok(())}).await.unwrap();}
            let job=state.store.get_pkuvideo_job(id).await.unwrap();
            assert_eq!(job.status,PipelineStatus::Succeeded);assert_eq!(job.charged_units,2);assert_eq!(job.result_source.as_deref(),Some("cache_hit"));
            assert_eq!(state.store.note_balance(user).await.unwrap(),expected_balance);
            handler.failed(id,"late failure".into()).await;
            assert_eq!(state.store.get_pkuvideo_job(id).await.unwrap().status,PipelineStatus::Succeeded);
        }
    }
    #[tokio::test]
    async fn missing_duration_waits_without_installing_and_recovers_once() {
        let temp=tempdir::TempDir::new("billing-duration-recovery").unwrap();
        let state=state(temp.path()).await;
        let (user,name,_cleanup)=cache_user(&state).await;
        let id=cache_job(&state,user,false).await;
        state.store.queue_note_billing(id,user).await.unwrap();
        assert!(state.store.check_note_start(id,user).await.unwrap());
        persist_delivery(&state,id,b"\\section{Private pending result}",None,"generated").await.unwrap();
        let handler=VideoTaskHandler{state:state.clone()};
        handler.completed(CompletionEvent{task_id:id,user_id:user,target:CompletionTarget::CourseLesson{username:name.clone(),course_id:"course".into(),lesson_id:"lesson".into()},result:Ok(())}).await.unwrap();
        let pending=state.store.get_pkuvideo_job(id).await.unwrap();
        assert_eq!(pending.billing_state.as_deref(),Some("BILLING_PENDING"));assert_eq!(pending.failure_code.as_deref(),Some("DURATION_UNAVAILABLE"));
        assert_eq!(state.store.note_balance(user).await.unwrap(),10);
        assert!(!lesson_storage::load_lessons(&state.store,&name,"course").await.unwrap()[0].is_processed);
        save_duration(&state,id,Some(3901)).await.unwrap();
        retry_pending_billing(&state).await;
        retry_pending_billing(&state).await;
        assert_eq!(state.store.note_balance(user).await.unwrap(),8);
        assert_eq!(state.store.get_pkuvideo_job(id).await.unwrap().status,PipelineStatus::Succeeded);
        assert!(installed(&state,&name).await.0.contains("Private pending result"));
    }
    #[tokio::test]
    async fn worker_rejects_zero_and_negative_before_cache_or_pipeline() {
        let temp=tempdir::TempDir::new("billing-worker-rejection").unwrap();
        let state=state(temp.path()).await;
        for balance in [0,-1] {
            let user=Uuid::new_v4();
            state.store.insert_user(crate::store::FileUser{id:user,username:user.to_string(),email:format!("{user}@example.invalid"),password_hash:String::new(),is_active:true,created_at:Utc::now(),updated_at:Utc::now(),storage_quota:3,processing_quota:balance,processing_priority:5,iaaa_encrypted_password:None,iaaa_public_key:None}).await.unwrap();
            let id=Uuid::new_v4();
            let mut job=super::super::pkuvideo::PkuVideoJob::new(id,user,"https://example.invalid/unreachable".into(),temp.path().join(id.to_string()).to_string_lossy().into());job.share_cache=true;
            state.store.insert_pkuvideo_job(job.clone()).await.unwrap();state.pkuvideo_jobs.write().await.insert(id,job);
            let handler=VideoTaskHandler{state:state.clone()};
            let error=handler.process(VideoTask{id,user_id:user,priority:5,submitted_at:Utc::now(),media:PreparedMedia{url:"https://example.invalid/unreachable".into(),enc_key:None},completion:CompletionTarget::None}).await.unwrap_err();
            assert!(error.starts_with(crate::store::note_billing::INSUFFICIENT));
            handler.failed(id,error).await;
            assert_eq!(state.store.get_pkuvideo_job(id).await.unwrap().failure_code.as_deref(),Some(crate::store::note_billing::INSUFFICIENT));
            assert_eq!(state.store.note_balance(user).await.unwrap(),balance);
        }
    }

    #[tokio::test]
    async fn cross_account_cache_regeneration_edits_failure_and_old_index() {
        let temp = tempdir::TempDir::new("cross-account-cache").unwrap();
        let state = state(temp.path()).await;
        let (a, an, _ac) = cache_user(&state).await;
        let (b, bn, _bc) = cache_user(&state).await;
        let h1 = "\\section{H1}";
        let h2 = "\\section{H2}";
        let h3 = "\\section{H3}";
        assert!(execute_fixture(&state, a, &an, false, Some(h1)).await.1);
        let (bid, called) = execute_fixture(&state, b, &bn, false, None).await;
        assert!(!called);
        assert_eq!(
            state
                .store
                .get_pkuvideo_job(bid)
                .await
                .unwrap()
                .result_source
                .as_deref(),
            Some("cache_hit")
        );
        let (text, base, path) = installed(&state, &an).await;
        assert_eq!(text, h1);
        assert_eq!(base, Some(crate::store::tex_hash(h1.as_bytes())));
        std::fs::write(path, "user A personal edits").unwrap();
        assert!(execute_fixture(&state, a, &an, true, Some(h2)).await.1);
        assert_eq!(installed(&state, &bn).await.0, h1);
        let (_, _, bpath) = installed(&state, &bn).await;
        std::fs::write(bpath, "user B personal edits").unwrap();
        assert!(!execute_fixture(&state, b, &bn, true, None).await.1);
        assert_eq!(installed(&state, &bn).await.0, h2);
        assert!(execute_fixture(&state, b, &bn, true, Some(h3)).await.1);
        assert_eq!(installed(&state, &an).await.0, h2);
        let (failed, called) = execute_fixture(&state, b, &bn, true, None).await;
        assert!(called);
        assert_eq!(
            state.store.get_pkuvideo_job(failed).await.unwrap().status,
            PipelineStatus::Failed
        );
        assert_eq!(installed(&state, &bn).await.0, h3);
        let key = crate::store::tex_hash(b"https://classroom.invalid/video?identity=exact");
        let (entry, bytes) = state.store.verified_tex(&key).await.unwrap().unwrap();
        assert_eq!(bytes, h3.as_bytes());
        assert_eq!(entry.revision, 3);
        let mut lessons = lesson_storage::load_lessons(&state.store, &an, "course")
            .await
            .unwrap();
        lessons[0].initial_tex_hash = None;
        lesson_storage::save_lessons(&state.store, &an, "course", &lessons)
            .await
            .unwrap();
        assert!(execute_fixture(&state, a, &an, true, Some(h1)).await.1);
        std::fs::write(entry.tex_path, "corruption").unwrap();
        assert!(execute_fixture(&state, b, &bn, false, Some(h2)).await.1);
        let observations = state.store.tex_cache_observations(&key).await.unwrap();
        assert_eq!(observations.as_array().unwrap().len(), 8);
    }

    #[tokio::test]
    async fn interrupted_installation_recovers_exact_private_result() {
        let temp = tempdir::TempDir::new("cache-install-recovery").unwrap();
        let state = state(temp.path()).await;
        let (user, name, _cleanup) = cache_user(&state).await;
        let id = cache_job(&state, user, false).await;
        mark_job_running(&state, id).await;
        persist_delivery(&state, id, b"\\section{Exact delivery}", None, "generated")
            .await
            .unwrap();
        state.load_pkuvideo_jobs().await;
        let job = state.store.get_pkuvideo_job(id).await.unwrap();
        assert_eq!(job.status, PipelineStatus::Succeeded);
        assert_eq!(
            installed(&state, &name).await.0,
            "\\section{Exact delivery}"
        );
        assert_eq!(installed(&state, &name).await.1, job.result_tex_sha256);
        let cache = state.store.verified_tex(job.url_key.as_deref().unwrap()).await.unwrap().unwrap();
        assert_eq!(cache.0.tex_sha256, job.result_tex_sha256.unwrap());
        assert_eq!(job.cache_revision_used, Some(cache.0.revision));
        state.load_pkuvideo_jobs().await;
        assert_eq!(
            state.store.get_pkuvideo_job(id).await.unwrap().status,
            PipelineStatus::Succeeded
        );
        let old = cache_job(&state, user, true).await;
        mark_job_running(&state, old).await;
        persist_delivery(&state, old, b"\\section{Old interrupted result}", None, "regenerated").await.unwrap();
        VideoTaskHandler { state:state.clone() }.failed(old,"installation failure".into()).await;
        assert!(execute_fixture(&state,user,&name,true,Some("\\section{Newer result}")).await.1);
        state.load_pkuvideo_jobs().await;
        assert_eq!(installed(&state,&name).await.0,"\\section{Newer result}");
        let old=state.store.get_pkuvideo_job(old).await.unwrap();
        assert_eq!(old.status,PipelineStatus::Failed);
        assert!(!old.postprocessing);
    }

    #[tokio::test]
    async fn completion_installs_notes_and_keeps_original_task_id_queryable() {
        let temp = tempdir::TempDir::new("queue-completion").unwrap();
        let state = state(temp.path()).await;
        let username = format!("queue_completion_test_{}", Uuid::new_v4().simple());
        let _cleanup = CourseFixture(PathBuf::from("./course_data").join(&username));
        let lessons: Vec<lesson_storage::LessonInfo> = serde_json::from_value(json!([{
            "id":"lesson","video_url":"","play_url":"","date":"2026-01-01","time":"08:00",
            "day_of_week":"","is_processed":false,"tex_filename":null
        }]))
        .unwrap();
        lesson_storage::save_lessons(&state.store, &username, "course", &lessons)
            .await
            .unwrap();
        let artifacts = temp.path().join("artifacts");
        std::fs::create_dir_all(&artifacts).unwrap();
        let notes = artifacts.join("notes.tex");
        std::fs::write(&notes, "generated notes").unwrap();
        let id = Uuid::new_v4();
        let user = Uuid::new_v4();
        let mut job = super::super::pkuvideo::PkuVideoJob::new(
            id,
            user,
            "url".into(),
            artifacts.to_string_lossy().into(),
        );
        job.status = PipelineStatus::Running;
        job.postprocessing = true;
        job.course_id = Some("course".into());
        job.lesson_id = Some("lesson".into());
        job.notes_path = Some(notes.to_string_lossy().into());
        state.store.insert_pkuvideo_job(job.clone()).await.unwrap();
        state.pkuvideo_jobs.write().await.insert(id, job);
        let handler = VideoTaskHandler {
            state: state.clone(),
        };
        handler
            .completed(CompletionEvent {
                task_id: id,
                user_id: user,
                result: Ok(()),
                target: CompletionTarget::CourseLesson {
                    username: username.clone(),
                    course_id: "course".into(),
                    lesson_id: "lesson".into(),
                },
            })
            .await
            .unwrap();
        let persisted = state.store.get_pkuvideo_job(id).await.unwrap();
        assert_eq!(persisted.status, PipelineStatus::Succeeded);
        assert!(state.pkuvideo_jobs.read().await.contains_key(&id));
        assert!(!persisted.postprocessing);
        assert_eq!(
            std::fs::read_to_string(persisted.course_notes_path.unwrap()).unwrap(),
            "generated notes"
        );
        assert!(!artifacts.exists());
        assert!(
            lesson_storage::load_lessons(&state.store, &username, "course")
                .await
                .unwrap()[0]
                .is_processed
        );
        assert!(!state.course_busy(user, "course").await);
    }

    #[tokio::test]
    async fn failed_completion_is_persisted_and_preserves_intermediate_results() {
        let temp = tempdir::TempDir::new("queue-failure").unwrap();
        let state = state(temp.path()).await;
        let id = Uuid::new_v4();
        let user = Uuid::new_v4();
        let artifacts = temp.path().join("artifacts");
        std::fs::create_dir_all(&artifacts).unwrap();
        let mut job = super::super::pkuvideo::PkuVideoJob::new(
            id,
            user,
            "url".into(),
            artifacts.to_string_lossy().into(),
        );
        job.status = PipelineStatus::Running;
        job.postprocessing = true;
        job.course_id = Some("course".into());
        job.lesson_id = Some("lesson".into());
        state.store.insert_pkuvideo_job(job.clone()).await.unwrap();
        state.pkuvideo_jobs.write().await.insert(id, job);
        let handler = VideoTaskHandler {
            state: state.clone(),
        };
        let result = handler
            .completed(CompletionEvent {
                task_id: id,
                user_id: user,
                result: Ok(()),
                target: CompletionTarget::CourseLesson {
                    username: "unused".into(),
                    course_id: "course".into(),
                    lesson_id: "lesson".into(),
                },
            })
            .await;
        handler.failed(id, result.unwrap_err()).await;
        let persisted = state.store.get_pkuvideo_job(id).await.unwrap();
        assert_eq!(persisted.status, PipelineStatus::Failed);
        assert!(persisted.error.unwrap().contains("课节后处理失败"));
        assert!(!persisted.postprocessing);
        assert!(artifacts.exists());
        assert!(!state.lesson_busy(user, "course", "lesson").await);
    }
}


async fn probe_duration(path: &std::path::Path) -> Option<i64> {
    let output=tokio::time::timeout(std::time::Duration::from_secs(30),tokio::process::Command::new("ffprobe")
        .args(["-v","error","-show_entries","format=duration","-of","default=noprint_wrappers=1:nokey=1"])
        .arg(path).kill_on_drop(true).output()).await.ok()?.ok()?;
    if !output.status.success() { return None; }
    crate::store::note_billing::duration_seconds(&String::from_utf8(output.stdout).ok()?).ok()
}
async fn save_duration(state:&AppState,id:Uuid,seconds:Option<i64>)->Result<(),String> {
    let mut jobs=state.pkuvideo_jobs.write().await;
    let job=jobs.get_mut(&id).ok_or("任务不存在")?;
    job.billing_duration_seconds=seconds;
    state.store.update_pkuvideo_job(job.clone()).await.map_err(|e|e.to_string())
}
async fn resolve_duration(state:&AppState,job:&super::pkuvideo::PkuVideoJob)->Option<i64> {
    if let Some(key)=&job.url_key {
        if let Ok(Some((entry,_)))=state.store.verified_tex(key).await {
            if let Some(seconds)=entry.video_duration_seconds.filter(|s|*s>0) {return Some(seconds);}
        }
    }
    for filename in ["video.mp4","audio.mp3","duration_probe.mp4"] {
        if let Some(seconds)=probe_duration(&PathBuf::from(&job.artifacts_dir).join(filename)).await {return Some(seconds);}
    }
    let media:Option<PreparedMedia>=state.store.load_sidecar(&PathBuf::from("note_billing_media").join(job.id.to_string())).await.ok().flatten();
    if let Some(media)=media {
        let path=PathBuf::from(&job.artifacts_dir).join("duration_probe.mp4");
        let _=tokio::fs::create_dir_all(&job.artifacts_dir).await;
        if super::pkuvideo::download_prepared(&media,&path,state.settings.pkuvideo.m3u8_concurrency,state.settings.pkuvideo.m3u8_segment_timeout_seconds,None).await.is_ok() {
            if let Some(seconds)=probe_duration(&path).await {
                if job.share_cache {if let Some(key)=&job.url_key {let _=state.store.set_tex_duration(key,seconds).await;}}
                return Some(seconds);
            }
        }
    }
    None
}
async fn billing_pending(state:&AppState,mut job:super::pkuvideo::PkuVideoJob,code:&str)->Result<(),String> {
    job.status=PipelineStatus::Running; job.postprocessing=true;job.billing_state=Some("BILLING_PENDING".into());
    job.failure_code=Some(code.into());job.error=Some("结果交付中，正在等待计费恢复".into());job.updated_at=Utc::now();job.finished_at=None;
    // Keep the private snapshot even when the database is temporarily unavailable.
    state.pkuvideo_jobs.write().await.insert(job.id,job.clone());
    state.store.set_note_billing_state(job.id,"BILLING_PENDING",Some(code)).await.map_err(|e|e.to_string())?;
    state.store.update_pkuvideo_job(job).await.map_err(|e|e.to_string())
}

pub async fn retry_pending_billing(state:&AppState) {
    let jobs:Vec<_>=state.pkuvideo_jobs.read().await.values().filter(|j|j.billing_state.as_deref()==Some("BILLING_PENDING") || j.failure_code.as_deref()==Some("BILLING_SYSTEM_UNAVAILABLE")).cloned().collect();
    let handler=VideoTaskHandler{state:state.clone()};
    for job in jobs {
        if job.failure_code.as_deref()==Some("BILLING_SYSTEM_UNAVAILABLE") {
            if let Ok(Some(media))=state.store.load_sidecar::<PreparedMedia>(&PathBuf::from("note_billing_media").join(job.id.to_string())).await {
                let _=enqueue_prepared_job(state,job.id,job.user_id,media).await;
            }
            continue;
        }
        let superseded=state.pkuvideo_jobs.read().await.values().any(|newer| newer.user_id==job.user_id && newer.created_at>job.created_at && newer.status==PipelineStatus::Succeeded && match (&job.course_id,&job.lesson_id) {
            (Some(course),Some(lesson))=>newer.course_id.as_ref()==Some(course)&&newer.lesson_id.as_ref()==Some(lesson),
            _=>newer.course_id.is_none()&&newer.url_key==job.url_key,
        });
        if superseded && job.course_notes_path.is_none() {
            let _=state.store.set_note_billing_state(job.id,"FAILED_NO_CHARGE",Some("DELIVERY_SUPERSEDED")).await;
            let mut cancelled=job.clone();cancelled.status=PipelineStatus::Failed;cancelled.postprocessing=false;cancelled.billing_state=Some("FAILED_NO_CHARGE".into());cancelled.failure_code=Some("DELIVERY_SUPERSEDED".into());cancelled.error=Some("后续任务已交付新版笔记，取消旧结果恢复".into());
            let _=state.store.update_pkuvideo_job(cancelled.clone()).await;
            state.pkuvideo_jobs.write().await.insert(cancelled.id,cancelled);
            continue;
        }
        let target=match (&job.course_id,&job.lesson_id) {
            (Some(course),Some(lesson))=> {
                let Some(user)=state.store.get_user(job.user_id).await else {continue;};
                CompletionTarget::CourseLesson{username:user.email.split('@').next().unwrap_or_default().into(),course_id:course.clone(),lesson_id:lesson.clone()}
            },
            _=>CompletionTarget::None,
        };
        if let Err(error)=handler.completed(CompletionEvent{task_id:job.id,user_id:job.user_id,target,result:Ok(())}).await {
            tracing::warn!(task_id=%job.id,%error,"计费恢复稍后重试");
        }
    }
}

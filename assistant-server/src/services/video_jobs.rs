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
        run_prepared_job(self.state.clone(), task.id, task.user_id, task.media).await
    }
    async fn completed(&self, event: CompletionEvent) -> Result<(), String> {
        event.result?;
        let user_id = event.user_id;
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
                    self.state
                        .store
                        .set_standalone_initial_hash(user_id, key, hash)
                        .await?;
                }
            }
            CompletionTarget::CourseLesson {
                username,
                course_id,
                lesson_id,
            } => {
                super::course_processing::complete_lesson(
                    &self.state,
                    event.task_id,
                    event.user_id,
                    &username,
                    &course_id,
                    &lesson_id,
                )
                .await
                .map_err(|error| format!("课节后处理失败: {error}"))?;
            }
        }
        let mut jobs = self.state.pkuvideo_jobs.write().await;
        let job = jobs.get_mut(&event.task_id).ok_or("任务不存在")?;
        job.status = PipelineStatus::Succeeded;
        job.error = None;
        job.postprocessing = false;
        job.updated_at = Utc::now();
        job.finished_at = Some(Utc::now());
        self.state
            .store
            .update_pkuvideo_job(job.clone())
            .await
            .map_err(|e| e.to_string())?;
        let artifacts = job.artifacts_dir.clone();
        let installed_course = job.course_id.is_some();
        drop(jobs);
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
            job.status = PipelineStatus::Failed;
            job.postprocessing = job.result_tex_sha256.is_some();
            job.error = Some(error);
            job.updated_at = Utc::now();
            job.finished_at = Some(Utc::now());
            if let Err(error) = self.state.store.update_pkuvideo_job(job.clone()).await {
                tracing::error!(%task_id,error=%error,"保存任务失败状态失败");
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
    let task = {
        let mut jobs = state.pkuvideo_jobs.write().await;
        let job = jobs.get_mut(&job_id).ok_or("任务不存在")?;
        if job.user_id != user_id {
            return Err("无权提交此任务".into());
        }
        job.priority = priority;
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
    let result = generate(job_dir, merged_settings, media).await;

    match result {
        Ok((transcript_path, notes_path, pdf_path)) => {
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
    let mut jobs = state.pkuvideo_jobs.write().await;
    if let Some(job) = jobs.get_mut(&job_id) {
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

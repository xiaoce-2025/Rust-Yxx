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
        match event.target {
            CompletionTarget::None => {}
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
        job.postprocessing = false;
        job.updated_at = Utc::now();
        self.state
            .store
            .update_pkuvideo_job(job.clone())
            .await
            .map_err(|e| e.to_string())
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
            job.postprocessing = false;
            job.error = Some(error);
            job.updated_at = Utc::now();
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
    mark_job_running(&state, job_id).await;
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
    let result = run_pipeline(
        job_dir,
        PathBuf::from("./data/pkuvideo_jobs"),
        &merged_settings,
        media,
    )
    .await;

    match result {
        Ok((transcript_path, notes_path, pdf_path)) => {
            mark_step_success(&state, job_id, 0, "视频下载完成").await;
            mark_step_success(&state, job_id, 1, "mp3 抽离完成").await;
            mark_step_success(&state, job_id, 2, "语音转文字完成").await;
            mark_step_success(&state, job_id, 3, "讲义提取完成").await;
            mark_step_success(&state, job_id, 4, "PDF 编译完成").await;

            let mut jobs = state.pkuvideo_jobs.write().await;
            if let Some(job) = jobs.get_mut(&job_id) {
                job.status = PipelineStatus::Running;
                job.postprocessing = job.course_id.is_some();
                job.updated_at = Utc::now();
                job.transcript_path = transcript_path;
                job.notes_path = notes_path;
                job.pdf_path = pdf_path;
                // 保存到FileStore
                let _ = state.store.update_pkuvideo_job(job.clone()).await;
            }
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
fn merge_with_user_config(
    global_settings: &Settings,
    user_config: &Option<crate::store::FileUserConfig>,
) -> Settings {
    let mut merged = global_settings.clone();

    if let Some(user_cfg) = user_config {
        // 阿里云配置覆盖
        if let Some(model) = &user_cfg.aliyun_model {
            if !model.is_empty() {
                merged.paraformer.model = Some(model.clone());
            }
        }
        if let Some(api_key) = &user_cfg.aliyun_api_key {
            if !api_key.is_empty() {
                merged.paraformer.api_key = Some(api_key.clone());
            }
        }
        // DeepSeek配置覆盖
        if let Some(model) = &user_cfg.deepseek_model {
            if !model.is_empty() {
                merged.deepseek.model = Some(model.clone());
            }
        }
        if let Some(api_key) = &user_cfg.deepseek_api_key {
            if !api_key.is_empty() {
                merged.deepseek.api_key = Some(api_key.clone());
            }
        }
    }

    merged
}

async fn mark_job_running(state: &AppState, job_id: Uuid) {
    let mut jobs = state.pkuvideo_jobs.write().await;
    if let Some(job) = jobs.get_mut(&job_id) {
        job.status = PipelineStatus::Running;
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
        lesson_storage::save_lessons(&username, "course", &lessons).unwrap();
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
        assert!(lesson_storage::load_lessons(&username, "course").unwrap()[0].is_processed);
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

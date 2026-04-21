use std::path::PathBuf;

use axum::{
    extract::{Path, State},
    routing::{get, post},
    Json, Router,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::task;
use uuid::Uuid;

use crate::{
    app_state::AppState,
    auth::{verify_password, CurrentUser},
    error::ApiError,
    services::pkuvideo::{run_pipeline, PipelineStatus, PkuVideoJob},
};

#[derive(Debug, Deserialize)]
struct SubmitApiRequest {
    platform_email: String,
    platform_password: String,
    pku_auth_one: String,
    pku_auth_two: String,
    video_url: String,
}

#[derive(Debug, Deserialize)]
struct SubmitWebRequest {
    pku_auth_one: String,
    pku_auth_two: String,
    video_url: String,
}

#[derive(Debug, Serialize)]
struct SubmitResponse {
    job_id: Uuid,
    status: String,
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/pkuvideo/submit", post(submit_api))
        .route("/api/pkuvideo/submit-web", post(submit_web))
        .route("/api/pkuvideo/jobs/:id", get(job_detail))
}

async fn submit_api(
    State(state): State<AppState>,
    Json(req): Json<SubmitApiRequest>,
) -> Result<Json<SubmitResponse>, ApiError> {
    validate_input(&req.video_url, &req.pku_auth_one, &req.pku_auth_two)?;

    let user = state
        .store
        .get_user_by_email(req.platform_email.trim())
        .await
        .ok_or(ApiError::Unauthorized)?;

    if !verify_password(req.platform_password.trim(), &user.password_hash)? {
        return Err(ApiError::Unauthorized);
    }

    let job_id = enqueue_job(
        state,
        user.id,
        req.video_url,
        req.pku_auth_one,
        req.pku_auth_two,
    )
    .await?;

    Ok(Json(SubmitResponse {
        job_id,
        status: "queued".to_string(),
    }))
}

async fn submit_web(
    State(state): State<AppState>,
    current_user: CurrentUser,
    Json(req): Json<SubmitWebRequest>,
) -> Result<Json<SubmitResponse>, ApiError> {
    validate_input(&req.video_url, &req.pku_auth_one, &req.pku_auth_two)?;

    let job_id = enqueue_job(
        state,
        current_user.id,
        req.video_url,
        req.pku_auth_one,
        req.pku_auth_two,
    )
    .await?;

    Ok(Json(SubmitResponse {
        job_id,
        status: "queued".to_string(),
    }))
}

async fn job_detail(
    State(state): State<AppState>,
    current_user: CurrentUser,
    Path(id): Path<Uuid>,
) -> Result<Json<PkuVideoJob>, ApiError> {
    let jobs = state.pkuvideo_jobs.read().await;
    let job = jobs.get(&id).cloned().ok_or(ApiError::NotFound)?;

    if job.user_id != current_user.id {
        return Err(ApiError::Forbidden);
    }

    Ok(Json(job))
}

fn validate_input(video_url: &str, pku_auth_one: &str, pku_auth_two: &str) -> Result<(), ApiError> {
    if video_url.trim().is_empty() || pku_auth_one.trim().is_empty() || pku_auth_two.trim().is_empty() {
        return Err(ApiError::BadRequest("video_url 和两条 pku 认证信息不能为空".to_string()));
    }
    if !(video_url.starts_with("http://") || video_url.starts_with("https://")) {
        return Err(ApiError::BadRequest("video_url 必须是 http/https 链接".to_string()));
    }
    Ok(())
}

async fn enqueue_job(
    state: AppState,
    user_id: Uuid,
    video_url: String,
    pku_auth_one: String,
    pku_auth_two: String,
) -> Result<Uuid, ApiError> {
    let job_id = Uuid::new_v4();
    let artifacts_dir = format!("./data/pkuvideo_jobs/{job_id}");

    let job = PkuVideoJob::new(job_id, user_id, video_url.clone(), artifacts_dir);
    state.pkuvideo_jobs.write().await.insert(job_id, job);

    let state_for_task = state.clone();
    task::spawn(async move {
        if let Err(e) = run_job_pipeline(
            state_for_task,
            job_id,
            video_url,
            pku_auth_one,
            pku_auth_two,
        )
        .await
        {
            let mut jobs = state.pkuvideo_jobs.write().await;
            if let Some(job) = jobs.get_mut(&job_id) {
                job.status = PipelineStatus::Failed;
                job.updated_at = Utc::now();
                job.error = Some(e);
            }
        }
    });

    Ok(job_id)
}

async fn run_job_pipeline(
    state: AppState,
    job_id: Uuid,
    video_url: String,
    pku_auth_one: String,
    pku_auth_two: String,
) -> Result<(), String> {
    mark_job_running(&state, job_id).await;

    mark_step_running(&state, job_id, 0, "开始下载视频").await;
    let base_dir = PathBuf::from("./data/pkuvideo_jobs");
    let result = run_pipeline(job_id, base_dir, video_url, pku_auth_one, pku_auth_two).await;

    match result {
        Ok((transcript_path, notes_path)) => {
            mark_step_success(&state, job_id, 0, "视频下载完成").await;
            mark_step_success(&state, job_id, 1, "mp3 抽离完成").await;
            mark_step_success(&state, job_id, 2, "语音转文字完成").await;
            mark_step_success(&state, job_id, 3, "讲义提取完成").await;

            let mut jobs = state.pkuvideo_jobs.write().await;
            if let Some(job) = jobs.get_mut(&job_id) {
                job.status = PipelineStatus::Succeeded;
                job.updated_at = Utc::now();
                job.transcript_path = transcript_path;
                job.notes_path = notes_path;
            }
            Ok(())
        }
        Err(err) => {
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
                return (index.min(3), message.to_string());
            }
        }
    }
    (0, raw.to_string())
}

async fn mark_job_running(state: &AppState, job_id: Uuid) {
    let mut jobs = state.pkuvideo_jobs.write().await;
    if let Some(job) = jobs.get_mut(&job_id) {
        job.status = PipelineStatus::Running;
        job.updated_at = Utc::now();
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
        job.updated_at = Utc::now();
    }
}

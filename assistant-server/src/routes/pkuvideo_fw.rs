use std::path::PathBuf;
use std::sync::atomic::AtomicU8;
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Path, State},
    routing::{delete, get, post},
    Json, Router,
};
use tokio_util::io::ReaderStream;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::task;
use uuid::Uuid;

use crate::{
    app_state::AppState,
    auth::CurrentUser,
    error::ApiError,
    services::pkuvideo_fw::{self, PipelineStatus, PkuVideoFwJob},
};

#[derive(Debug, Deserialize)]
struct SubmitRequest {
    pku_auth_one: String,
    pku_auth_two: String,
    video_url: String,
    manual_key: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CheckM3u8Request {
    video_url: String,
    pku_auth_one: String,
    pku_auth_two: String,
}

#[derive(Debug, Serialize)]
struct SubmitResponse {
    job_id: Uuid,
    status: String,
}

#[derive(Debug, Serialize)]
struct JobDetailResponse {
    id: Uuid,
    user_id: Uuid,
    created_at: String,
    updated_at: String,
    status: PipelineStatus,
    video_url: String,
    video_path: Option<String>,
    error: Option<String>,
    progress: Option<u8>,
}

impl JobDetailResponse {
    fn from_job(job: &PkuVideoFwJob) -> Self {
        Self {
            id: job.id,
            user_id: job.user_id,
            created_at: job.created_at.to_rfc3339(),
            updated_at: job.updated_at.to_rfc3339(),
            status: job.status.clone(),
            video_url: job.video_url.clone(),
            video_path: job.video_path.clone(),
            error: job.error.clone(),
            progress: job.get_progress(),
        }
    }
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/pkuvideo_fw/submit", post(submit))
        .route("/api/pkuvideo_fw/check-m3u8", post(check_m3u8))
        .route("/api/pkuvideo_fw/jobs/:id", get(job_detail))
        .route("/api/pkuvideo_fw/jobs", get(job_list))
        .route("/api/pkuvideo_fw/jobs/:id", delete(delete_job))
        .route("/api/pkuvideo_fw/jobs/:id/video", get(download_video))
}

async fn submit(
    State(state): State<AppState>,
    current_user: CurrentUser,
    Json(req): Json<SubmitRequest>,
) -> Result<Json<SubmitResponse>, ApiError> {
    validate_input(&req.video_url, &req.pku_auth_one, &req.pku_auth_two)?;

    // 检查用户是否已有任务（每个用户只有一个槽位）
    {
        let jobs = state.pkuvideo_fw_jobs.read().await;
        let has_existing = jobs.values().any(|j| j.user_id == current_user.id);
        if has_existing {
            return Err(ApiError::Forbidden(
                "您已有进行中或已完成的视频任务，请先删除后再提交新任务".to_string(),
            ));
        }
    }

    // 校验 IAAA 账号与邮箱学号是否匹配
    let user = state
        .store
        .get_user(current_user.id)
        .await
        .ok_or(ApiError::Unauthorized)?;
    let email_student_id = user.email.split('@').next().unwrap_or("").trim();
    if email_student_id != req.pku_auth_one.trim() {
        return Err(ApiError::BadRequest(
            "IAAA账号与注册邮箱学号不匹配，请检查".to_string(),
        ));
    }

    let job_id = Uuid::new_v4();
    let progress_tracker = Arc::new(AtomicU8::new(0));
    let mut job = PkuVideoFwJob::new(job_id, current_user.id, req.video_url.clone());
    job.progress = Some(progress_tracker.clone());

    state
        .pkuvideo_fw_jobs
        .write()
        .await
        .insert(job_id, job.clone());

    // 保存到FileStore
    state
        .store
        .insert_pkuvideo_fw_job(job)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let state_for_task = state.clone();
    let state_for_error = state.clone();
    let user_id_for_task = current_user.id;
    task::spawn(async move {
        if let Err(e) = run_download_job(
            state_for_task,
            job_id,
            user_id_for_task,
            req.video_url,
            req.pku_auth_one,
            req.pku_auth_two,
            req.manual_key,
            progress_tracker,
        )
        .await
        {
            let mut jobs = state_for_error.pkuvideo_fw_jobs.write().await;
            if let Some(job) = jobs.get_mut(&job_id) {
                job.status = PipelineStatus::Failed;
                job.updated_at = Utc::now();
                job.error = Some(e);
                let _ = state_for_error
                    .store
                    .update_pkuvideo_fw_job(job.clone())
                    .await;
            }
        }
    });

    Ok(Json(SubmitResponse {
        job_id,
        status: "queued".to_string(),
    }))
}

async fn check_m3u8(
    State(state): State<AppState>,
    current_user: CurrentUser,
    Json(req): Json<CheckM3u8Request>,
) -> Result<Json<serde_json::Value>, ApiError> {
    validate_input(&req.video_url, &req.pku_auth_one, &req.pku_auth_two)?;

    if !req.video_url.contains(".m3u8") {
        return Ok(Json(serde_json::json!({"is_m3u8": false})));
    }

    match pkuvideo_fw::fetch_m3u8_key_uri(
        &req.video_url,
        &req.pku_auth_one,
        &req.pku_auth_two,
        &state.login_cache,
    )
    .await
    {
        Ok(key_uri) => Ok(Json(serde_json::json!({
            "is_m3u8": true,
            "key_uri": key_uri,
        }))),
        Err(e) => Ok(Json(serde_json::json!({
            "is_m3u8": true,
            "key_uri": "",
            "error": e,
        }))),
    }
}

async fn job_detail(
    State(state): State<AppState>,
    current_user: CurrentUser,
    Path(id): Path<Uuid>,
) -> Result<Json<JobDetailResponse>, ApiError> {
    let jobs = state.pkuvideo_fw_jobs.read().await;
    let job = jobs.get(&id).ok_or(ApiError::NotFound)?;

    if job.user_id != current_user.id {
        return Err(ApiError::Forbidden("无权访问".to_string()));
    }

    Ok(Json(JobDetailResponse::from_job(job)))
}

async fn job_list(
    State(state): State<AppState>,
    current_user: CurrentUser,
) -> Result<Json<Vec<JobDetailResponse>>, ApiError> {
    let jobs = state.pkuvideo_fw_jobs.read().await;
    let user_jobs: Vec<JobDetailResponse> = jobs
        .values()
        .filter(|job| job.user_id == current_user.id)
        .map(|j| JobDetailResponse::from_job(j))
        .collect();

    Ok(Json(user_jobs))
}

async fn delete_job(
    State(state): State<AppState>,
    current_user: CurrentUser,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let job_info = {
        let jobs = state.pkuvideo_fw_jobs.read().await;
        let job = jobs.get(&id).ok_or(ApiError::NotFound)?;

        if job.user_id != current_user.id {
            return Err(ApiError::Forbidden("无权访问".to_string()));
        }

        job.video_path.clone()
    };

    // 删除任务目录中的文件
    if let Some(vp) = job_info {
        let artifacts_path = PathBuf::from(&vp)
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("./data/pkuvideo_fw_jobs"));

        if artifacts_path.exists() {
            if let Err(e) = tokio::fs::remove_dir_all(&artifacts_path).await {
                eprintln!("删除任务目录失败: {}", e);
            }
        }
    }

    // 从内存中删除
    {
        let mut jobs = state.pkuvideo_fw_jobs.write().await;
        jobs.remove(&id);
    }

    // 从文件存储中删除
    state
        .store
        .delete_pkuvideo_fw_job(id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    Ok(Json(serde_json::json!({
        "success": true,
        "message": "任务删除成功"
    })))
}

fn validate_input(
    video_url: &str,
    pku_auth_one: &str,
    pku_auth_two: &str,
) -> Result<(), ApiError> {
    if video_url.trim().is_empty()
        || pku_auth_one.trim().is_empty()
        || pku_auth_two.trim().is_empty()
    {
        return Err(ApiError::BadRequest(
            "video_url 和两条 pku 认证信息不能为空".to_string(),
        ));
    }
    if !(video_url.starts_with("http://") || video_url.starts_with("https://")) {
        return Err(ApiError::BadRequest(
            "video_url 必须是 http/https 链接".to_string(),
        ));
    }
    Ok(())
}

async fn run_download_job(
    state: AppState,
    job_id: Uuid,
    _user_id: Uuid,
    video_url: String,
    pku_auth_one: String,
    pku_auth_two: String,
    manual_key: Option<String>,
    progress: Arc<AtomicU8>,
) -> Result<(), String> {
    // 标记为运行中
    {
        let mut jobs = state.pkuvideo_fw_jobs.write().await;
        if let Some(job) = jobs.get_mut(&job_id) {
            job.status = PipelineStatus::Running;
            job.updated_at = Utc::now();
        }
    }

    let base_dir = PathBuf::from("./data/pkuvideo_fw_jobs");
    let result = pkuvideo_fw::download_video_only(
        job_id,
        base_dir,
        video_url,
        pku_auth_one,
        pku_auth_two,
        &state.login_cache,
        manual_key.as_deref(),
        state.settings.pkuvideo.m3u8_concurrency,
        state.settings.pkuvideo.m3u8_segment_timeout_seconds,
        Some(progress),
    )
    .await;

    match result {
        Ok(video_path) => {
            let mut jobs = state.pkuvideo_fw_jobs.write().await;
            if let Some(job) = jobs.get_mut(&job_id) {
                job.status = PipelineStatus::Succeeded;
                job.updated_at = Utc::now();
                job.video_path = Some(video_path.to_string_lossy().to_string());
                let _ = state.store.update_pkuvideo_fw_job(job.clone()).await;
            }
            Ok(())
        }
        Err(err) => {
            let message = if let Some((_, msg)) = err.split_once(';') {
                msg.to_string()
            } else {
                err.clone()
            };
            let mut jobs = state.pkuvideo_fw_jobs.write().await;
            if let Some(job) = jobs.get_mut(&job_id) {
                job.status = PipelineStatus::Failed;
                job.updated_at = Utc::now();
                job.error = Some(message.clone());
                let _ = state.store.update_pkuvideo_fw_job(job.clone()).await;
            }
            Err(message)
        }
    }
}

async fn download_video(
    State(state): State<AppState>,
    current_user: CurrentUser,
    Path(job_id): Path<Uuid>,
) -> Result<axum::response::Response, ApiError> {
    let video_full_path = {
        let jobs = state.pkuvideo_fw_jobs.read().await;
        let job = jobs.get(&job_id).ok_or(ApiError::NotFound)?;

        if job.user_id != current_user.id {
            return Err(ApiError::Forbidden("无权访问此视频".to_string()));
        }

        job.video_path
            .clone()
            .ok_or(ApiError::NotFound)?
    };

    let file = tokio::fs::File::open(&video_full_path)
        .await
        .map_err(|_| ApiError::NotFound)?;
    let metadata = file.metadata().await.map_err(|_| ApiError::NotFound)?;
    let stream = ReaderStream::new(file);
    let body = Body::from_stream(stream);

    let filename = format!("video_{}.mp4", job_id);
    let response = axum::response::Response::builder()
        .header("Content-Type", "video/mp4")
        .header(
            "Content-Disposition",
            format!("attachment; filename=\"{}\"", filename),
        )
        .header("Content-Length", metadata.len())
        .body(body)
        .map_err(|_| ApiError::Internal("构建响应失败".to_string()))?;

    Ok(response)
}
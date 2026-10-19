use std::path::PathBuf;
use std::sync::atomic::AtomicU8;
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{self, HeaderMap, StatusCode},
    routing::{delete, get, post},
    Json, Router,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
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
    merging: bool,
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
            merging: job.status == PipelineStatus::Running
                && job.get_progress().is_some_and(|p| p >= 100),
            progress: job.get_progress().map(|p| {
                if matches!(
                    job.status,
                    PipelineStatus::Pending | PipelineStatus::Running
                ) {
                    p.min(99)
                } else {
                    p
                }
            }),
        }
    }
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/pkuvideo_fw/submit", post(submit))
        .route("/api/pkuvideo_fw/manual-replay", post(submit_manual_replay))
        .route("/api/pkuvideo_fw/check-m3u8", post(check_m3u8))
        .route("/api/pkuvideo_fw/jobs/:id", get(job_detail))
        .route("/api/pkuvideo_fw/jobs", get(job_list))
        .route("/api/pkuvideo_fw/jobs/:id", delete(delete_job))
        .route("/api/pkuvideo_fw/jobs/:id/video", get(serve_video))
        .route("/api/pkuvideo_fw/jobs/:id/video.mp4", get(serve_video))
}

async fn submit(
    State(state): State<AppState>,
    current_user: CurrentUser,
    Json(req): Json<SubmitRequest>,
) -> Result<Json<SubmitResponse>, ApiError> {
    let slot = state.fw_slot(current_user.id).await;
    let _guard = slot.lock().await;
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

#[derive(Deserialize)]
struct ManualReplayRequest {
    course_id: String,
    lesson_id: String,
}

async fn submit_manual_replay(
    State(state): State<AppState>,
    current: CurrentUser,
    Json(request): Json<ManualReplayRequest>,
) -> Result<Json<SubmitResponse>, ApiError> {
    let slot = state.fw_slot(current.id).await;
    let _guard = slot.lock().await;
    let user = state
        .store
        .get_user(current.id)
        .await
        .ok_or(ApiError::Unauthorized)?;
    let username = user.email.split('@').next().unwrap_or_default();
    let lessons = crate::services::lesson_storage::load_lessons(username, &request.course_id)
        .map_err(ApiError::BadRequest)?;
    let lesson = lessons
        .iter()
        .find(|l| l.id == request.lesson_id)
        .ok_or(ApiError::NotFound)?;
    let mut replay = lesson
        .manual_replay
        .clone()
        .ok_or_else(|| ApiError::BadRequest("只有手动添加课节可以通过好学 App 观看".into()))?;
    if replay.date.is_empty() {
        replay.date = lesson.date.clone();
    }
    let old_jobs: Vec<_> = state
        .pkuvideo_fw_jobs
        .read()
        .await
        .values()
        .filter(|j| j.user_id == current.id)
        .cloned()
        .collect();
    if old_jobs
        .iter()
        .any(|j| matches!(j.status, PipelineStatus::Pending | PipelineStatus::Running))
    {
        return Err(ApiError::Conflict(
            "已有视频正在下载，请等待完成后再观看其他回放".into(),
        ));
    }
    // Both download sources use the same single slot. Clear only this user's
    // terminal jobs after validating the lesson, before reserving the new slot.
    for job in old_jobs {
        remove_job(&state, job.id).await?;
    }
    let id = Uuid::new_v4();
    let mut job = PkuVideoFwJob::new(id, current.id, String::new());
    job.progress = Some(Arc::new(AtomicU8::new(0)));
    state
        .store
        .insert_pkuvideo_fw_job(job.clone())
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let progress = job.progress.clone();
    state.pkuvideo_fw_jobs.write().await.insert(id, job);
    task::spawn(async move {
        if let Some(job) = state.pkuvideo_fw_jobs.write().await.get_mut(&id) {
            job.status = PipelineStatus::Running;
            job.updated_at = Utc::now();
        }
        let result: Result<PathBuf, String> = async {
            let client = state.app_replay_client(current.id).await?;
            let mut client = client.lock().await;
            let login = client.login_bound(&user, &serde_json::json!({})).await?;
            if login["ok"] != true {
                return Err(login["error"]
                    .as_str()
                    .unwrap_or("好学认证失败，请返回课节页完成验证")
                    .into());
            }
            let dir = PathBuf::from("./data/pkuvideo_fw_jobs").join(id.to_string());
            tokio::fs::create_dir_all(&dir)
                .await
                .map_err(|_| "创建回放目录失败")?;
            let path = dir.join("video.mp4");
            client
                .download_with_progress(&replay, &path, progress)
                .await?;
            Ok(path)
        }
        .await;
        let mut jobs = state.pkuvideo_fw_jobs.write().await;
        if let Some(job) = jobs.get_mut(&id) {
            job.updated_at = Utc::now();
            match result {
                Ok(path) => {
                    job.status = PipelineStatus::Succeeded;
                    job.video_path = Some(path.to_string_lossy().into());
                    if let Some(progress) = &job.progress {
                        progress.store(100, std::sync::atomic::Ordering::SeqCst);
                    }
                }
                Err(error) => {
                    tracing::error!(target:"app_replay_download",job_id=%id,%error,"manual replay viewing failed");
                    job.status = PipelineStatus::Failed;
                    job.error = Some(error);
                }
            }
            if let Err(error) = state.store.update_pkuvideo_fw_job(job.clone()).await {
                tracing::error!(job_id=%id,%error,"saving replay job failed");
            }
        }
    });
    Ok(Json(SubmitResponse {
        job_id: id,
        status: "queued".into(),
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
    let slot = state.fw_slot(current_user.id).await;
    let _guard = slot.lock().await;
    {
        let jobs = state.pkuvideo_fw_jobs.read().await;
        let job = jobs.get(&id).ok_or(ApiError::NotFound)?;
        if job.user_id != current_user.id {
            return Err(ApiError::Forbidden("无权访问".into()));
        }
        if matches!(
            job.status,
            PipelineStatus::Pending | PipelineStatus::Running
        ) {
            return Err(ApiError::Conflict("视频正在下载，请等待完成后删除".into()));
        }
    }
    remove_job(&state, id).await?;
    Ok(Json(serde_json::json!({
        "success": true,
        "message": "任务删除成功"
    })))
}

pub(crate) async fn remove_job(state: &AppState, id: Uuid) -> Result<(), ApiError> {
    crate::services::video_translation::remove(state, id)
        .await
        .map_err(ApiError::Internal)?;
    remove_job_files(std::path::Path::new("./data/pkuvideo_fw_jobs"), id).await?;
    crate::services::video_translation::finish_remove(state, id)
        .await
        .map_err(ApiError::Internal)?;
    state
        .store
        .delete_pkuvideo_fw_job(id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    state.pkuvideo_fw_jobs.write().await.remove(&id);
    Ok(())
}
async fn remove_job_files(root: &std::path::Path, id: Uuid) -> Result<(), ApiError> {
    if root.exists() {
        let root = tokio::fs::canonicalize(root)
            .await
            .map_err(|_| ApiError::Internal("读取回放存储目录失败".into()))?;
        let mut entries = tokio::fs::read_dir(&root)
            .await
            .map_err(|_| ApiError::Internal("读取回放目录失败".into()))?;
        let suffix = format!("_{id}");
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|_| ApiError::Internal("读取回放目录失败".into()))?
        {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == id.to_string() || name.ends_with(&suffix) {
                let path = tokio::fs::canonicalize(entry.path())
                    .await
                    .map_err(|_| ApiError::Internal("读取旧回放路径失败".into()))?;
                if path == root || !path.starts_with(&root) {
                    return Err(ApiError::Internal("旧回放路径超出存储目录".into()));
                }
                tokio::fs::remove_dir_all(path)
                    .await
                    .map_err(|_| ApiError::Internal("删除旧回放文件失败".into()))?;
            }
        }
    }
    Ok(())
}
fn validate_input(video_url: &str, pku_auth_one: &str, pku_auth_two: &str) -> Result<(), ApiError> {
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

#[derive(Debug, Deserialize)]
struct VideoQuery {
    download: Option<String>,
}

async fn serve_video(
    State(state): State<AppState>,
    current_user: CurrentUser,
    Path(job_id): Path<Uuid>,
    Query(query): Query<VideoQuery>,
    headers: HeaderMap,
) -> Result<axum::response::Response, ApiError> {
    let video_full_path = {
        let jobs = state.pkuvideo_fw_jobs.read().await;
        let job = jobs.get(&job_id).ok_or(ApiError::NotFound)?;

        if job.user_id != current_user.id {
            return Err(ApiError::Forbidden("无权访问此视频".to_string()));
        }

        job.video_path.clone().ok_or(ApiError::NotFound)?
    };

    let mut file = tokio::fs::File::open(&video_full_path)
        .await
        .map_err(|_| ApiError::NotFound)?;
    let metadata = file.metadata().await.map_err(|_| ApiError::NotFound)?;
    let file_size = metadata.len();

    let filename = format!("video_{}.mp4", job_id);
    let force_download = query.download.as_deref() == Some("1");

    // Parse Range header
    if let Some(range_header) = headers.get(http::header::RANGE) {
        if let Ok(range_str) = range_header.to_str() {
            if let Some((start, end)) = parse_byte_range(range_str, file_size) {
                let length = end - start + 1;

                file.seek(SeekFrom::Start(start))
                    .await
                    .map_err(|_| ApiError::Internal("seek失败".to_string()))?;

                let mut handle = file.take(length);
                let mut buf = vec![0u8; length as usize];
                handle
                    .read_exact(&mut buf)
                    .await
                    .map_err(|_| ApiError::Internal("读取视频数据失败".to_string()))?;

                let response = axum::response::Response::builder()
                    .status(StatusCode::PARTIAL_CONTENT)
                    .header("Content-Type", "video/mp4")
                    .header(
                        "Content-Range",
                        format!("bytes {}-{}/{}", start, end, file_size),
                    )
                    .header("Content-Length", length)
                    .header("Accept-Ranges", "bytes")
                    .body(Body::from(buf))
                    .map_err(|_| ApiError::Internal("构建响应失败".to_string()))?;

                return Ok(response);
            }
        }
    }

    // Full file response (no Range, or invalid Range)
    let mut buf = Vec::with_capacity(file_size as usize);
    file.read_to_end(&mut buf)
        .await
        .map_err(|_| ApiError::Internal("读取视频文件失败".to_string()))?;

    let mut builder = axum::response::Response::builder()
        .header("Content-Type", "video/mp4")
        .header("Accept-Ranges", "bytes")
        .header("Content-Length", file_size);

    if force_download {
        builder = builder.header(
            "Content-Disposition",
            format!("attachment; filename=\"{}\"", filename),
        );
    } else {
        builder = builder.header(
            "Content-Disposition",
            format!("inline; filename=\"{}\"", filename),
        );
    }

    builder
        .body(Body::from(buf))
        .map_err(|_| ApiError::Internal("构建响应失败".to_string()))
}

fn parse_byte_range(range_str: &str, file_size: u64) -> Option<(u64, u64)> {
    let range_str = range_str.strip_prefix("bytes=")?;
    let parts: Vec<&str> = range_str.split(',').collect();
    let first = parts.first()?;
    let range_parts: Vec<&str> = first.split('-').collect();

    if range_parts.len() != 2 {
        return None;
    }

    let start: u64 = if range_parts[0].is_empty() {
        // suffix range: "bytes=-500" means last 500 bytes
        let suffix: u64 = range_parts[1].parse().ok()?;
        if suffix > file_size {
            return Some((0, file_size - 1));
        }
        return Some((file_size - suffix, file_size - 1));
    } else {
        range_parts[0].parse().ok()?
    };

    let end: u64 = if range_parts[1].is_empty() {
        // open-ended: "bytes=0-"
        file_size - 1
    } else {
        range_parts[1].parse().ok()?
    };

    if start > end || start >= file_size {
        return None;
    }

    let end = end.min(file_size - 1);
    Some((start, end))
}

#[cfg(test)]
mod replay_view_tests {
    #[test]
    fn completed_segments_show_ninety_nine_until_merging_finishes() {
        let mut job =
            super::PkuVideoFwJob::new(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), String::new());
        job.status = super::PipelineStatus::Running;
        job.progress = Some(std::sync::Arc::new(std::sync::atomic::AtomicU8::new(50)));
        let detail = super::JobDetailResponse::from_job(&job);
        assert_eq!(detail.progress, Some(50));
        assert!(!detail.merging);
        job.progress
            .as_ref()
            .unwrap()
            .store(100, std::sync::atomic::Ordering::SeqCst);
        let detail = super::JobDetailResponse::from_job(&job);
        assert_eq!(detail.progress, Some(99));
        assert!(detail.merging);
        job.status = super::PipelineStatus::Succeeded;
        let detail = super::JobDetailResponse::from_job(&job);
        assert_eq!(detail.progress, Some(100));
        assert!(!detail.merging);
    }
    #[tokio::test]
    async fn replacing_video_removes_only_that_jobs_files() {
        let dir = tempdir::TempDir::new_in("target", "fw-replace").unwrap();
        let old = uuid::Uuid::new_v4();
        let other = uuid::Uuid::new_v4();
        for name in [
            old.to_string(),
            format!("20261014_120000_{old}"),
            other.to_string(),
            "unrelated".into(),
        ] {
            std::fs::create_dir(dir.path().join(&name)).unwrap();
            std::fs::write(dir.path().join(name).join("video.mp4"), b"test").unwrap();
        }
        super::remove_job_files(dir.path(), old).await.unwrap();
        assert!(!dir.path().join(old.to_string()).exists());
        assert!(!dir.path().join(format!("20261014_120000_{old}")).exists());
        assert!(dir
            .path()
            .join(other.to_string())
            .join("video.mp4")
            .exists());
        assert!(dir.path().join("unrelated").exists());
    }
}

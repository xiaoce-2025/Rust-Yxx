use std::path::PathBuf;

use axum::{
    extract::{Path, State},
    routing::{delete, get, post},
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
        .route("/api/pkuvideo/jobs", get(job_list))
        .route("/api/pkuvideo/jobs/:id/reexecute", post(reexecute_step))
        .route("/api/pkuvideo/jobs/:id", delete(delete_job))
        .route("/api/pkuvideo/jobs/:id/pdf", get(download_pdf))
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
        return Err(ApiError::Forbidden("无权访问".to_string()));
    }

    Ok(Json(job))
}

async fn job_list(
    State(state): State<AppState>,
    current_user: CurrentUser,
) -> Result<Json<Vec<PkuVideoJob>>, ApiError> {
    let jobs = state.pkuvideo_jobs.read().await;
    let user_jobs: Vec<PkuVideoJob> = jobs
        .values()
        .filter(|job| job.user_id == current_user.id)
        .cloned()
        .collect();

    Ok(Json(user_jobs))
}

#[derive(Debug, Deserialize)]
struct ReexecuteStepRequest {
    step: u8,
}

async fn reexecute_step(
    State(state): State<AppState>,
    current_user: CurrentUser,
    Path(id): Path<Uuid>,
    Json(req): Json<ReexecuteStepRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut jobs = state.pkuvideo_jobs.write().await;
    let job = jobs.get_mut(&id).ok_or(ApiError::NotFound)?;

    if job.user_id != current_user.id {
        return Err(ApiError::Forbidden("无权访问".to_string()));
    }

    // 构建任务目录路径（时间戳_任务ID）
    let timestamp = job.created_at.format("%Y%m%d_%H%M%S").to_string();
    let base_dir = PathBuf::from("./data/pkuvideo_jobs");
    let job_dir = base_dir.join(format!("{}_{}", timestamp, id));

    let video_path = job_dir.join("video.mp4");
    let mp3_path = job_dir.join("audio.mp3");
    let transcript_path = job_dir.join("transcript.txt");
    let notes_path = job_dir.join("lecture_notes.tex");
    let pdf_path = job_dir.join("lecture_notes.pdf");

    // 根据步骤索引重新执行
    match req.step {
        0 => {
            // 重新执行 视频下载（需要原始URL）
            crate::services::pkuvideo::download_video(
                &job.video_url,
                &video_path,
                "",
                "",
            )
                .await
                .map_err(|e| ApiError::Internal(e))?;
            job.steps[0].status = crate::services::pkuvideo::PipelineStatus::Succeeded;
            job.steps[0].message = "视频下载完成".to_string();
            job.steps[0].finished_at = Some(chrono::Utc::now());
        }
        1 => {
            // 重新执行 MP3 抽离
            crate::services::pkuvideo::extract_mp3(&video_path, &mp3_path)
                .await
                .map_err(|e| ApiError::Internal(e))?;
            job.steps[1].status = crate::services::pkuvideo::PipelineStatus::Succeeded;
            job.steps[1].message = "MP3 抽离完成".to_string();
            job.steps[1].finished_at = Some(chrono::Utc::now());
        }
        2 => {
            // 重新执行 语音转文字
            crate::services::pkuvideo::speech_to_text(&mp3_path, &transcript_path, &state.settings)
                .await
                .map_err(|e| ApiError::Internal(e))?;
            job.steps[2].status = crate::services::pkuvideo::PipelineStatus::Succeeded;
            job.steps[2].message = "语音转文字完成".to_string();
            job.steps[2].finished_at = Some(chrono::Utc::now());
            // 存储相对路径
            let transcript_rel = transcript_path.strip_prefix(&base_dir)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| transcript_path.to_string_lossy().to_string());
            job.transcript_path = Some(transcript_rel);
        }
        3 => {
            // 重新执行 讲义提取
            crate::services::pkuvideo::extract_notes(&transcript_path, &notes_path, &state.settings)
                .await
                .map_err(|e| ApiError::Internal(e))?;
            job.steps[3].status = crate::services::pkuvideo::PipelineStatus::Succeeded;
            job.steps[3].message = "讲义提取完成".to_string();
            job.steps[3].finished_at = Some(chrono::Utc::now());
            // 存储相对路径
            let notes_rel = notes_path.strip_prefix(&base_dir)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| notes_path.to_string_lossy().to_string());
            job.notes_path = Some(notes_rel);
        }
        4 => {
            // 重新执行 PDF 编译
            crate::services::pkuvideo::compile_pdf(&notes_path, &pdf_path)
                .await
                .map_err(|e| ApiError::Internal(e))?;
            job.steps[4].status = crate::services::pkuvideo::PipelineStatus::Succeeded;
            job.steps[4].message = "PDF 编译完成".to_string();
            job.steps[4].finished_at = Some(chrono::Utc::now());
            // 存储相对路径
            let pdf_rel = pdf_path.strip_prefix(&base_dir)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| pdf_path.to_string_lossy().to_string());
            job.pdf_path = Some(pdf_rel);
        }
        _ => {
            return Err(ApiError::BadRequest("无效的步骤索引".to_string()));
        }
    }

    job.updated_at = chrono::Utc::now();
    job.status = crate::services::pkuvideo::PipelineStatus::Succeeded;
    
    // 保存到FileStore
    state.store.update_pkuvideo_job(job.clone()).await.map_err(|e| ApiError::Internal(e.to_string()))?;

    Ok(Json(serde_json::json!({
        "success": true,
        "message": format!("步骤 {} 重新执行成功", req.step)
    })))
}

async fn delete_job(
    State(state): State<AppState>,
    current_user: CurrentUser,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // 先获取任务信息
    let job_info = {
        let jobs = state.pkuvideo_jobs.read().await;
        let job = jobs.get(&id).ok_or(ApiError::NotFound)?;

        if job.user_id != current_user.id {
            return Err(ApiError::Forbidden("无权访问".to_string()));
        }

        // 复制需要的信息
        (job.pdf_path.clone(), job.status.clone())
    };

    let (pdf_path, job_status) = job_info;

    // 删除任务目录中的文件（通过 pdf_path 的父目录定位）
    if let Some(pdf) = pdf_path {
        let artifacts_path = PathBuf::from(&pdf).parent()
            .map(|p| std::path::Path::new("./data/pkuvideo_jobs").join(p))
            .unwrap_or_else(|| PathBuf::from("./data/pkuvideo_jobs"));
        
        if artifacts_path.exists() {
            if let Err(e) = tokio::fs::remove_dir_all(&artifacts_path).await {
                eprintln!("删除任务目录失败: {}", e);
            }
        }
    }

    // 从内存中删除任务
    {
        let mut jobs = state.pkuvideo_jobs.write().await;
        jobs.remove(&id);
    }

    // 从文件存储中删除任务
    state.store.delete_pkuvideo_job(id).await.map_err(|e| ApiError::Internal(e.to_string()))?;

    // 如果任务已完成，返还储存限额
    if job_status == PipelineStatus::Succeeded {
        state.store.increase_storage_quota(current_user.id, 1).await.map_err(|e| ApiError::Internal(e.to_string()))?;
    }

    Ok(Json(serde_json::json!({
        "success": true,
        "message": "任务删除成功"
    })))
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
    // 获取用户信息，校验学号与邮箱匹配
    let user = state.store.get_user(user_id).await.ok_or(ApiError::Unauthorized)?;
    
    // 从邮箱中提取 @ 前的学号部分
    let email_student_id = user.email.split('@').next().unwrap_or("").trim();
    
    // 校验 IAAA 账号与邮箱学号是否匹配
    if email_student_id != pku_auth_one.trim() {
        return Err(ApiError::BadRequest("IAAA账号与注册邮箱学号不匹配，请检查".to_string()));
    }
    
    // 先检查是否有相同视频链接的已完成任务
    {
        let existing_jobs = state.pkuvideo_jobs.read().await;
        if let Some(existing_job) = existing_jobs.values().find(|job| {
            job.user_id == user_id && 
            job.video_url == video_url && 
            job.status == PipelineStatus::Succeeded &&
            job.pdf_path.is_some()
        }) {
            // 找到相同视频链接的已完成任务，直接复制结果（不消耗限额）
            return copy_existing_job(&state, user_id, &video_url, existing_job).await;
        }
    } // 在这里释放读取锁

    // 检查用户限额
    if user.processing_quota <= 0 {
        return Err(ApiError::Forbidden("处理资源限额不足，请联系管理员".to_string()));
    }

    // 检查储存资源限额
    if user.storage_quota <= 0 {
        return Err(ApiError::Forbidden("储存资源限额不足，请删除已完成的任务释放空间".to_string()));
    }

    // 扣除处理限额
    state.store.decrease_processing_quota(user_id).await.map_err(|e| ApiError::Internal(e.to_string()))?;

    // 扣除储存资源限额
    state.store.decrease_storage_quota(user_id, 1).await.map_err(|e| ApiError::Internal(e.to_string()))?;

    let job_id = Uuid::new_v4();
    let artifacts_dir = format!("./data/pkuvideo_jobs/{job_id}");

    let job = PkuVideoJob::new(job_id, user_id, video_url.clone(), artifacts_dir);
    state.pkuvideo_jobs.write().await.insert(job_id, job.clone());
    
    // 保存到FileStore
    state.store.insert_pkuvideo_job(job).await.map_err(|e| ApiError::Internal(e.to_string()))?;

    let state_for_task = state.clone();
    let state_for_error = state.clone();
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
            let mut jobs = state_for_error.pkuvideo_jobs.write().await;
            if let Some(job) = jobs.get_mut(&job_id) {
                job.status = PipelineStatus::Failed;
                job.updated_at = Utc::now();
                job.error = Some(e);
                // 保存到FileStore
                let _ = state_for_error.store.update_pkuvideo_job(job.clone()).await;
            }
        }
    });

    Ok(job_id)
}

async fn copy_existing_job(
    state: &AppState,
    user_id: Uuid,
    video_url: &str,
    existing_job: &PkuVideoJob,
) -> Result<Uuid, ApiError> {
    let job_id = Uuid::new_v4();
    let artifacts_dir = format!("./data/pkuvideo_jobs/{job_id}");
    
    // 创建新任务目录
    std::fs::create_dir_all(&artifacts_dir).map_err(|e| ApiError::Internal(e.to_string()))?;

    // 复制转写文件
    let mut new_transcript_path = None;
    if let Some(transcript_path) = &existing_job.transcript_path {
        let src_path = PathBuf::from(transcript_path);
        let dest_path = PathBuf::from(&artifacts_dir).join(src_path.file_name().unwrap());
        std::fs::copy(&src_path, &dest_path).map_err(|e| ApiError::Internal(e.to_string()))?;
        new_transcript_path = Some(dest_path.to_string_lossy().to_string());
    }

    // 复制讲义文件
    let mut new_notes_path = None;
    if let Some(notes_path) = &existing_job.notes_path {
        let src_path = PathBuf::from(notes_path);
        let dest_path = PathBuf::from(&artifacts_dir).join(src_path.file_name().unwrap());
        std::fs::copy(&src_path, &dest_path).map_err(|e| ApiError::Internal(e.to_string()))?;
        new_notes_path = Some(dest_path.to_string_lossy().to_string());
    }

    // 复制PDF文件
    let mut new_pdf_path = None;
    if let Some(pdf_path) = &existing_job.pdf_path {
        let src_path = PathBuf::from(pdf_path);
        let dest_path = PathBuf::from(&artifacts_dir).join(src_path.file_name().unwrap());
        std::fs::copy(&src_path, &dest_path).map_err(|e| ApiError::Internal(e.to_string()))?;
        new_pdf_path = Some(dest_path.to_string_lossy().to_string());
    }

    // 创建新任务并标记为成功
    let mut job = PkuVideoJob::new(job_id, user_id, video_url.to_string(), artifacts_dir);
    job.status = PipelineStatus::Succeeded;
    job.transcript_path = new_transcript_path;
    job.notes_path = new_notes_path;
    job.pdf_path = new_pdf_path;
    
    // 标记所有步骤为成功
    job.steps[0].status = PipelineStatus::Succeeded;
    job.steps[0].message = "视频下载完成(复制)".to_string();
    job.steps[0].finished_at = Some(Utc::now());
    
    job.steps[1].status = PipelineStatus::Succeeded;
    job.steps[1].message = "MP3抽离完成(复制)".to_string();
    job.steps[1].finished_at = Some(Utc::now());
    
    job.steps[2].status = PipelineStatus::Succeeded;
    job.steps[2].message = "语音转文字完成(复制)".to_string();
    job.steps[2].finished_at = Some(Utc::now());
    
    job.steps[3].status = PipelineStatus::Succeeded;
    job.steps[3].message = "讲义提取完成(复制)".to_string();
    job.steps[3].finished_at = Some(Utc::now());
    
    job.steps[4].status = PipelineStatus::Succeeded;
    job.steps[4].message = "PDF编译完成(复制)".to_string();
    job.steps[4].finished_at = Some(Utc::now());

    job.updated_at = Utc::now();

    // 保存到内存和文件存储
    state.pkuvideo_jobs.write().await.insert(job_id, job.clone());
    state.store.insert_pkuvideo_job(job).await.map_err(|e| ApiError::Internal(e.to_string()))?;

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
    let result = run_pipeline(job_id, base_dir, video_url, pku_auth_one, pku_auth_two, &state.settings).await;

    match result {
        Ok((transcript_path, notes_path, pdf_path)) => {
            mark_step_success(&state, job_id, 0, "视频下载完成").await;
            mark_step_success(&state, job_id, 1, "mp3 抽离完成").await;
            mark_step_success(&state, job_id, 2, "语音转文字完成").await;
            mark_step_success(&state, job_id, 3, "讲义提取完成").await;
            mark_step_success(&state, job_id, 4, "PDF 编译完成").await;

            let mut jobs = state.pkuvideo_jobs.write().await;
            if let Some(job) = jobs.get_mut(&job_id) {
                job.status = PipelineStatus::Succeeded;
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
        job.updated_at = Utc::now();
        job.error = Some(msg.to_string());
        // 保存到FileStore
        let _ = state.store.update_pkuvideo_job(job.clone()).await;
    }
}

async fn download_pdf(
    State(state): State<AppState>,
    current_user: CurrentUser,
    Path(job_id): Path<Uuid>,
) -> Result<axum::response::Response, ApiError> {
    let pdf_path_relative = {
        let jobs = state.pkuvideo_jobs.read().await;
        let job = jobs.get(&job_id).ok_or(ApiError::NotFound)?;

        if job.user_id != current_user.id {
            return Err(ApiError::Forbidden("无权访问此任务的 PDF".to_string()));
        }

        job.pdf_path.clone().ok_or(ApiError::NotFound)?
    };

    let pdf_full_path = std::path::Path::new("./data/pkuvideo_jobs").join(&pdf_path_relative);
    
    let pdf_data = tokio::fs::read(&pdf_full_path)
        .await
        .map_err(|_| ApiError::NotFound)?;

    let filename = format!("lecture_notes_{}.pdf", job_id);
    let response = axum::response::Response::builder()
        .header("Content-Type", "application/pdf")
        .header("Content-Disposition", format!("attachment; filename=\"{}\"", filename))
        .header("Content-Length", pdf_data.len())
        .body(axum::body::Body::from(pdf_data))
        .map_err(|_| ApiError::Internal("构建响应失败".to_string()))?;

    Ok(response)
}

use std::path::PathBuf;

use axum::{
    extract::{Path, State},
    routing::{delete, get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    app_state::AppState,
    auth::{verify_password, CurrentUser},
    error::ApiError,
    services::pkuvideo::{PipelineStatus, PkuVideoJob},
};

#[derive(Debug, Deserialize)]
struct SubmitApiRequest {
    platform_email: String,
    platform_password: String,
    #[serde(default)]
    pku_auth_one: String,
    #[serde(default)]
    pku_auth_two: String,
    video_url: String,
    manual_key: Option<String>,
    #[serde(default)]
    regenerate: bool,
}

#[derive(Debug, Deserialize)]
struct SubmitWebRequest {
    #[serde(default)]
    pku_auth_one: String,
    #[serde(default)]
    pku_auth_two: String,
    video_url: String,
    manual_key: Option<String>,
    #[serde(default)]
    regenerate: bool,
}

#[derive(Debug, Deserialize)]
struct CheckM3u8Request {
    video_url: String,
    #[serde(default)]
    pku_auth_one: String,
    #[serde(default)]
    pku_auth_two: String,
}

#[derive(Debug, Serialize)]
struct SubmitResponse {
    job_id: Uuid,
    status: String,
    status_url: String,
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/pkuvideo/submit", post(submit_api))
        .route("/api/pkuvideo/submit-web", post(submit_web))
        .route("/api/pkuvideo/check-m3u8", post(check_m3u8))
        .route("/api/pkuvideo/jobs/:id", get(job_detail))
        .route("/api/pkuvideo/jobs", get(job_list))
        .route("/api/pkuvideo/jobs/:id/reexecute", post(reexecute_step))
        .route("/api/pkuvideo/jobs/:id", delete(delete_job))
        .route("/api/pkuvideo/jobs/:id/pdf", get(download_pdf))
        .route("/api/pkuvideo/jobs/:id/tex", get(download_tex))
}

async fn submit_api(
    State(state): State<AppState>,
    Json(req): Json<SubmitApiRequest>,
) -> Result<Json<SubmitResponse>, ApiError> {
    validate_input(&req.video_url)?;

    let user = state
        .store
        .get_user_by_email(req.platform_email.trim())
        .await
        .ok_or(ApiError::Unauthorized)?;

    if !verify_password(req.platform_password.trim(), &user.password_hash)? {
        return Err(ApiError::Unauthorized);
    }

    // This API also supports password authentication without a session cookie.
    let gate = state.batch_submissions.gate(user.id).await;
    let _operation = gate.read_owned().await;
    if state.batch_submissions.active(user.id).await {
        return Err(ApiError::Locked("任务正在提交中，请等待批量提交完成后再操作".into()));
    }

    let job_id = enqueue_job(
        state,
        user.id,
        req.video_url,
        req.pku_auth_one,
        req.pku_auth_two,
        req.manual_key,
        req.regenerate,
    )
    .await?;

    Ok(Json(SubmitResponse {
        job_id,
        status: "queued".to_string(),
        status_url: format!("/api/pkuvideo/jobs/{job_id}"),
    }))
}

async fn submit_web(
    State(state): State<AppState>,
    current_user: CurrentUser,
    Json(req): Json<SubmitWebRequest>,
) -> Result<Json<SubmitResponse>, ApiError> {
    validate_input(&req.video_url)?;

    let job_id = enqueue_job(
        state,
        current_user.id,
        req.video_url,
        req.pku_auth_one,
        req.pku_auth_two,
        req.manual_key,
        req.regenerate,
    )
    .await?;

    Ok(Json(SubmitResponse {
        job_id,
        status: "queued".to_string(),
        status_url: format!("/api/pkuvideo/jobs/{job_id}"),
    }))
}

async fn check_m3u8(
    State(state): State<AppState>,
    current_user: CurrentUser,
    Json(req): Json<CheckM3u8Request>,
) -> Result<Json<serde_json::Value>, ApiError> {
    validate_input(&req.video_url)?;

    if !req.video_url.contains(".m3u8") {
        return Ok(Json(serde_json::json!({"is_m3u8": false})));
    }

    match crate::services::pkuvideo::fetch_m3u8_key_uri(
        &req.video_url,
        &req.pku_auth_one,
        &req.pku_auth_two,
        &state.login_cache,
    ).await {
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
) -> Result<Json<serde_json::Value>, ApiError> {
    let jobs = state.pkuvideo_jobs.read().await;
    let job = jobs.get(&id).cloned().ok_or(ApiError::NotFound)?;

    if job.user_id != current_user.id {
        return Err(ApiError::Forbidden("无权访问".to_string()));
    }

    Ok(Json(public_job(&job)))
}

async fn job_list(
    State(state): State<AppState>,
    current_user: CurrentUser,
) -> Result<Json<Vec<serde_json::Value>>, ApiError> {
    let jobs = state.pkuvideo_jobs.read().await;
    let mut user_jobs: Vec<PkuVideoJob> = jobs
        .values()
        .filter(|job| job.user_id == current_user.id)
        .cloned()
        .collect();

    user_jobs.sort_by_key(|j|std::cmp::Reverse(j.created_at));
    Ok(Json(user_jobs.iter().map(public_job).collect()))
}

// Never serialize internal URLs, keys or local filesystem paths to normal users.
fn public_job(job: &PkuVideoJob) -> serde_json::Value {
    let steps: Vec<_> = job.steps.iter().map(|step|serde_json::json!({
        "name":step.name,"status":step.status,"started_at":step.started_at,"finished_at":step.finished_at,
        "message":if step.status==PipelineStatus::Failed { "处理失败，请重试或联系管理员" } else { step.message.as_str() }
    })).collect();
    serde_json::json!({"id":job.id,"created_at":job.created_at,"updated_at":job.updated_at,
        "status":job.status,"steps":steps,"error":job.error.as_ref().map(|_| match job.failure_code.as_deref() {
            Some("INSUFFICIENT_NOTE_CREDITS")=>"处理失败 · 额度不足：课堂笔记处理额度不足，请联系管理员添加额度",
            Some("DURATION_UNAVAILABLE"|"BILLING_COMMIT_FAILED")=>"结果交付中，正在等待计费恢复",
            _=>"处理失败，请重试或联系管理员"
        }),"billing_state":job.billing_state,"charged_units":job.charged_units,"billing_duration_seconds":job.billing_duration_seconds,"failure_code":job.failure_code,
        "course_id":job.course_id,"lesson_id":job.lesson_id,"course_name":job.course_name,
        "course_date":job.course_date,"regenerate":job.regenerate,"priority":job.priority,
        "postprocessing":job.postprocessing,"started_at":job.started_at,"finished_at":job.finished_at,
        "result_source":job.result_source,"video_url":"已隐藏源链接",
        "pdf_path":if job.status == PipelineStatus::Succeeded { Some(format!("/api/pkuvideo/jobs/{}/pdf",job.id)) } else { None },
        "tex_url":if job.status == PipelineStatus::Succeeded { Some(format!("/api/pkuvideo/jobs/{}/tex",job.id)) } else { None }})
}

#[derive(Debug, Deserialize)]
struct ReexecuteStepRequest {
    step: u8,
    manual_key: Option<String>,
}

async fn reexecute_step(
    State(state): State<AppState>, current_user: CurrentUser, Path(id): Path<Uuid>,
    Json(req): Json<ReexecuteStepRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if req.step > 4 { return Err(ApiError::BadRequest("无效的步骤索引".into())); }
    let job = state.pkuvideo_jobs.read().await.get(&id).cloned().ok_or(ApiError::NotFound)?;
    if job.user_id != current_user.id { return Err(ApiError::Forbidden("无权访问".into())); }
    if matches!(job.status, PipelineStatus::Pending | PipelineStatus::Running) || job.course_id.is_some() {
        return Err(ApiError::Conflict("活动任务不能重执行；课节任务请通过课节重新处理接口提交".into()));
    }
    let user = state.store.get_user(current_user.id).await.ok_or(ApiError::Unauthorized)?;
    let job_id = enqueue_job(state, current_user.id, job.video_url,
        user.email.split('@').next().unwrap_or_default().into(),
        user.iaaa_encrypted_password.unwrap_or_default(), req.manual_key, true).await?;
    Ok(Json(serde_json::json!({"success":true,"job_id":job_id,"status":"queued",
        "status_url":format!("/api/pkuvideo/jobs/{job_id}"),"message":"已加入重新生成队列"})))
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
        (job.artifacts_dir.clone(), job.status.clone(), job.course_id.is_none())
    };

    let (artifacts_dir, job_status, charged_storage) = job_info;
    if matches!(job_status, PipelineStatus::Pending | PipelineStatus::Running) {
        return Err(ApiError::Conflict("排队或处理中的任务不能删除".into()));
    }

    if let (Ok(root),Ok(target)) = (
        tokio::fs::canonicalize("./data/pkuvideo_jobs").await,
        tokio::fs::canonicalize(&artifacts_dir).await,
    ) {
        if target.starts_with(&root) && target != root {
            tokio::fs::remove_dir_all(target).await.map_err(|e|ApiError::Internal(e.to_string()))?;
        }
    }

    let _ = tokio::fs::remove_file(state.store.user_tex_path(current_user.id, id)).await;

    // 从内存中删除任务
    {
        let mut jobs = state.pkuvideo_jobs.write().await;
        jobs.remove(&id);
    }

    // 从文件存储中删除任务
    state.store.delete_pkuvideo_job(id).await.map_err(|e| ApiError::Internal(e.to_string()))?;

    // 如果任务已完成，返还储存限额
    if charged_storage && job_status == PipelineStatus::Succeeded {
        state.store.increase_storage_quota(current_user.id, 1).await.map_err(|e| ApiError::Internal(e.to_string()))?;
    }

    Ok(Json(serde_json::json!({
        "success": true,
        "message": "任务删除成功"
    })))
}

fn validate_input(video_url: &str) -> Result<(), ApiError> {
    if video_url.trim().is_empty() {
        return Err(ApiError::BadRequest("video_url 不能为空".to_string()));
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
    manual_key: Option<String>,
    regenerate: bool,
) -> Result<Uuid, ApiError> {
    // 获取用户信息，校验学号与邮箱匹配
    let user = state.store.get_user(user_id).await.ok_or(ApiError::Unauthorized)?;
    
    // 从邮箱中提取 @ 前的学号部分
    let email_student_id = user.email.split('@').next().unwrap_or("").trim();
    
    // 校验 IAAA 账号与邮箱学号是否匹配
    if !pku_auth_one.trim().is_empty() && email_student_id != pku_auth_one.trim() {
        return Err(ApiError::BadRequest("IAAA账号与注册邮箱学号不匹配，请检查".to_string()));
    }
    
    // 检查储存资源限额
    if user.storage_quota <= 0 {
        return Err(ApiError::Forbidden("储存资源限额不足，请删除已完成的任务释放空间".to_string()));
    }

    let media = crate::services::prepared_media::prepare_teaching(&video_url, &pku_auth_one,
        &pku_auth_two, &state.login_cache, manual_key.as_deref()).await.map_err(ApiError::BadRequest)?;

    // 扣除储存资源限额
    state.store.decrease_storage_quota(user_id, 1).await.map_err(|e| ApiError::Internal(e.to_string()))?;

    let job_id = Uuid::new_v4();
    let artifacts_dir = format!("./data/pkuvideo_jobs/{job_id}");

    let mut job = PkuVideoJob::new(job_id, user_id, media.url.clone(), artifacts_dir);
    job.regenerate = regenerate;
    // Arbitrary direct URLs may contain private media. They stay outside sharing.
    // Classroom discovery uses the authorized course submission path instead.
    state.pkuvideo_jobs.write().await.insert(job_id, job.clone());
    
    // 保存到FileStore
    if let Err(error)=state.store.insert_pkuvideo_job(job).await {
        state.pkuvideo_jobs.write().await.remove(&job_id);
        let _=state.store.increase_storage_quota(user_id,1).await;
        return Err(ApiError::Internal(error.to_string()));
    }

    if let Err(error) = crate::services::video_jobs::enqueue_prepared_job(&state, job_id, user_id, media).await {
        let _ = state.store.increase_storage_quota(user_id, 1).await;
        state.pkuvideo_jobs.write().await.remove(&job_id);
        let _ = state.store.delete_pkuvideo_job(job_id).await;
        let _ = state.store.set_note_billing_state(job_id,"FAILED_NO_CHARGE",Some("ENQUEUE_FAILED")).await;
        let _ = state.store.delete_sidecar(&PathBuf::from("note_billing_media").join(job_id.to_string())).await;
        return Err(ApiError::Internal(error));
    }

    Ok(job_id)
}

async fn download_pdf(
    State(state): State<AppState>,
    current_user: CurrentUser,
    Path(job_id): Path<Uuid>,
) -> Result<axum::response::Response, ApiError> {
    let job = owned_completed_job(&state, current_user.id, job_id).await?;
    let notes = private_note_path(&job)?;
    let pdf_data = compile_note_pdf(&notes).await?;

    let filename = format!("lecture_notes_{}.pdf", job_id);
    let response = axum::response::Response::builder()
        .header("Content-Type", "application/pdf")
        .header("Content-Disposition", format!("attachment; filename=\"{}\"", filename))
        .header("Content-Length", pdf_data.len())
        .body(axum::body::Body::from(pdf_data))
        .map_err(|_| ApiError::Internal("构建响应失败".to_string()))?;

    Ok(response)
}

async fn owned_completed_job(state: &AppState, user: Uuid, id: Uuid) -> Result<PkuVideoJob, ApiError> {
    let job = state.pkuvideo_jobs.read().await.get(&id).cloned().ok_or(ApiError::NotFound)?;
    if job.user_id != user { return Err(ApiError::Forbidden("无权访问".into())); }
    if job.status != PipelineStatus::Succeeded { return Err(ApiError::Conflict("任务尚未成功完成".into())); }
    Ok(job)
}

pub(super) fn private_note_path(job: &PkuVideoJob) -> Result<PathBuf, ApiError> {
    if let Some(path) = &job.course_notes_path { return Ok(PathBuf::from(path)); }
    let path = job.notes_path.as_ref().ok_or(ApiError::NotFound)?;
    Ok(PathBuf::from("./data/pkuvideo_jobs").join(path))
}

pub(super) async fn compile_note_pdf(path: &std::path::Path) -> Result<Vec<u8>, ApiError> {
    let temp = tempdir::TempDir::new("note-pdf").map_err(|e|ApiError::Internal(e.to_string()))?;
    let notes = temp.path().join("notes.tex");
    tokio::fs::copy(path,&notes).await.map_err(|_|ApiError::NotFound)?;
    let pdf = temp.path().join("notes.pdf");
    crate::services::pkuvideo::compile_pdf(&notes,&pdf).await.map_err(ApiError::Internal)?;
    tokio::fs::read(pdf).await.map_err(|e|ApiError::Internal(e.to_string()))
}

async fn download_tex(State(state): State<AppState>, current: CurrentUser, Path(id): Path<Uuid>) -> Result<axum::response::Response, ApiError> {
    let job = owned_completed_job(&state,current.id,id).await?;
    let bytes = tokio::fs::read(private_note_path(&job)?).await.map_err(|_|ApiError::NotFound)?;
    axum::response::Response::builder().header("Content-Type","application/x-tex; charset=utf-8")
        .header("Content-Disposition",format!("attachment; filename=\"lecture_notes_{id}.tex\""))
        .body(axum::body::Body::from(bytes)).map_err(|e|ApiError::Internal(e.to_string()))
}

#[cfg(test)]
mod credential_optional_tests {
    use super::*;

    #[test]
    fn manual_submission_and_detection_accept_no_iaaa_fields() {
        let payload = serde_json::json!({
            "video_url": "https://example.invalid/playlist.m3u8",
            "manual_key": "0123456789abcdef"
        });
        let submit: SubmitWebRequest = serde_json::from_value(payload.clone()).unwrap();
        assert!(submit.pku_auth_one.is_empty());
        assert!(submit.pku_auth_two.is_empty());
        assert!(validate_input(&submit.video_url).is_ok());
        let check: CheckM3u8Request = serde_json::from_value(payload).unwrap();
        assert!(validate_input(&check.video_url).is_ok());
        assert!(validate_input("").is_err());
        assert!(validate_input("file:///tmp/video.m3u8").is_err());
    }
}

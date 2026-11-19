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
        .route("/api/pkuvideo/standalone-jobs", get(standalone_job_list))
        .route("/api/video-task-history", get(task_history))
        .route("/api/pkuvideo/jobs/:id/reexecute", post(reexecute_step))
        .route("/api/pkuvideo/jobs/:id", delete(delete_job))
        .route("/api/pkuvideo/jobs/:id/pdf", get(download_pdf))
        .route("/api/pkuvideo/jobs/:id/tex", get(download_tex))
}

async fn submit_api(
    State(state): State<AppState>,
    audit: Option<axum::Extension<super::operations::AuditId>>,
    Json(req): Json<SubmitApiRequest>,
) -> Result<axum::response::Response, ApiError> {
    validate_input(&req.video_url)?;

    let user = state
        .store
        .get_user_by_email(req.platform_email.trim())
        .await
        .ok_or(ApiError::Unauthorized)?;

    if !user.is_active || !verify_password(req.platform_password.trim(), &user.password_hash)? {
        return Err(ApiError::Unauthorized);
    }
    if let Some(axum::Extension(audit)) = audit {
        state.store.attribute_audit(audit.0, Some(user.id), None).await
            .map_err(|e| ApiError::Internal(e.to_string()))?;
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

    Ok(super::operations::job_response(job_id, Json(SubmitResponse {
        job_id,
        status: "queued".to_string(),
        status_url: format!("/api/pkuvideo/jobs/{job_id}"),
    })))
}

async fn submit_web(
    State(state): State<AppState>,
    current_user: CurrentUser,
    Json(req): Json<SubmitWebRequest>,
) -> Result<axum::response::Response, ApiError> {
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

    Ok(super::operations::job_response(job_id, Json(SubmitResponse {
        job_id,
        status: "queued".to_string(),
        status_url: format!("/api/pkuvideo/jobs/{job_id}"),
    })))
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
    let job = state.pkuvideo_jobs.read().await.get(&id).cloned().ok_or(ApiError::NotFound)?;

    if job.user_id != current_user.id {
        return Err(ApiError::Forbidden("无权访问".to_string()));
    }

    let mut record = public_job(&job);
    if job.status == PipelineStatus::Pending {
        let waits = super::acceleration::queue_waits(&state, current_user.id).await.unwrap_or_default();
        record["estimated_wait_minutes"] = serde_json::json!(waits.get(&id).map(|wait| wait.minutes));
    }
    Ok(Json(record))
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

async fn standalone_job_list(State(state):State<AppState>,current:CurrentUser)->Json<Vec<serde_json::Value>> {
    let jobs=state.pkuvideo_jobs.read().await;
    let mut jobs:Vec<_>=jobs.values().filter(|job|job.user_id==current.id && job.is_stored_standalone()).collect();
    jobs.sort_by_key(|job|std::cmp::Reverse(job.created_at));
    Json(jobs.into_iter().map(public_job).collect())
}

async fn task_history(State(state):State<AppState>,current:CurrentUser)->Result<Json<Vec<serde_json::Value>>,ApiError> {
    let waits = super::acceleration::queue_waits(&state, current.id).await?;
    let jobs=state.pkuvideo_jobs.read().await;
    let mut jobs:Vec<_>=jobs.values().filter(|job|job.user_id==current.id).collect();
    jobs.sort_by_key(|job|std::cmp::Reverse(job.created_at));
    Ok(Json(jobs.into_iter().map(|job| {
        let mut record=public_job(job);
        if let Some(record)=record.as_object_mut() {
            for key in ["pdf_path","tex_url","video_url","steps"] {record.remove(key);}
            let wait = waits.get(&job.id).filter(|_| job.status == PipelineStatus::Pending);
            record.insert("can_accelerate".into(), serde_json::json!(wait.is_some_and(|entry| entry.can_accelerate)));
            record.insert("estimated_wait_minutes".into(), serde_json::json!(wait.map(|entry| entry.minutes)));
            record.insert("acceleration_used".into(), serde_json::json!(job.acceleration_used));
        }
        record
    }).collect()))
}

// Never serialize internal URLs, keys or local filesystem paths to normal users.
fn public_job(job: &PkuVideoJob) -> serde_json::Value {
    let steps: Vec<_> = job.steps.iter().map(|step|serde_json::json!({
        "name":step.name,"status":step.status,"started_at":step.started_at,"finished_at":step.finished_at,
        "message":if step.status==PipelineStatus::Failed { "处理失败，请重试或联系管理员" } else { step.message.as_str() }
    })).collect();
    serde_json::json!({"id":job.id,"created_at":job.created_at,"updated_at":job.updated_at,
        "status":job.status,"artifacts_deleted_at":job.artifacts_deleted_at,"steps":steps,"error":job.error.as_ref().map(|_| match job.failure_code.as_deref() {
            Some(crate::store::note_billing::INSUFFICIENT)=>crate::store::note_billing::INSUFFICIENT_MESSAGE,
            Some("DURATION_UNAVAILABLE"|"BILLING_COMMIT_FAILED")=>"结果交付中，正在等待计费恢复",
            _=>"处理失败，请重试或联系管理员"
        }),"billing_state":job.billing_state,"charged_units":job.charged_units,"billing_duration_seconds":job.billing_duration_seconds,"failure_code":job.failure_code,
        "course_id":job.course_id,"lesson_id":job.lesson_id,"course_name":job.course_name,
        "course_date":job.course_date,"regenerate":job.regenerate,"priority":job.priority,
        "postprocessing":job.postprocessing,"started_at":job.started_at,"finished_at":job.finished_at,
        "result_source":job.result_source,"video_url":"已隐藏源链接",
        "pdf_path":if job.status == PipelineStatus::Succeeded && job.artifacts_deleted_at.is_none() { Some(format!("/api/pkuvideo/jobs/{}/pdf",job.id)) } else { None },
        "tex_url":if job.status == PipelineStatus::Succeeded && job.artifacts_deleted_at.is_none() { Some(format!("/api/pkuvideo/jobs/{}/tex",job.id)) } else { None }})
}

#[derive(Debug, Deserialize)]
struct ReexecuteStepRequest {
    step: u8,
    manual_key: Option<String>,
}

async fn reexecute_step(
    State(state): State<AppState>, current_user: CurrentUser, Path(id): Path<Uuid>,
    Json(req): Json<ReexecuteStepRequest>,
) -> Result<axum::response::Response, ApiError> {
    if req.step > 4 { return Err(ApiError::BadRequest("无效的步骤索引".into())); }
    let job = state.pkuvideo_jobs.read().await.get(&id).cloned().ok_or(ApiError::NotFound)?;
    if job.user_id != current_user.id { return Err(ApiError::Forbidden("无权访问".into())); }
    if job.artifacts_deleted_at.is_some() { return Err(ApiError::NotFound); }
    if matches!(job.status, PipelineStatus::Pending | PipelineStatus::Running) || job.course_id.is_some() {
        return Err(ApiError::Conflict("活动任务不能重执行；课节任务请通过课节重新处理接口提交".into()));
    }
    let user = state.store.get_user(current_user.id).await.ok_or(ApiError::Unauthorized)?;
    let job_id = enqueue_job(state, current_user.id, job.video_url,
        user.email.split('@').next().unwrap_or_default().into(),
        user.iaaa_encrypted_password.unwrap_or_default(), req.manual_key, true).await?;
    Ok(super::operations::job_response(job_id, Json(serde_json::json!({"success":true,"job_id":job_id,"status":"queued",
        "status_url":format!("/api/pkuvideo/jobs/{job_id}"),"message":"已加入重新生成队列"}))))
}

async fn delete_job(
    State(state): State<AppState>,
    current_user: CurrentUser,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let job=state.pkuvideo_jobs.read().await.get(&id).cloned().ok_or(ApiError::NotFound)?;
    if job.user_id!=current_user.id {return Err(ApiError::Forbidden("无权访问".into()));}
    if job.course_id.is_some() {return Err(ApiError::BadRequest("课程笔记请在课程页面管理".into()));}
    if matches!(job.status,PipelineStatus::Pending|PipelineStatus::Running) {
        return Err(ApiError::Conflict("排队或处理中的任务不能删除".into()));
    }
    // Remove standalone files first. A failed cleanup must not free a storage slot.
    if let (Ok(root),Ok(target))=(tokio::fs::canonicalize("./data/pkuvideo_jobs").await,tokio::fs::canonicalize(&job.artifacts_dir).await) {
        if target.starts_with(&root) && target!=root {
            match tokio::fs::remove_dir_all(target).await {
                Ok(())=>{},
                Err(error) if error.kind()==std::io::ErrorKind::NotFound=>{},
                Err(error)=>return Err(ApiError::Internal(error.to_string())),
            }
        }
    }
    match tokio::fs::remove_file(state.store.user_tex_path(current_user.id,id)).await {
        Ok(())=>{},
        Err(error) if error.kind()==std::io::ErrorKind::NotFound=>{},
        Err(error)=>return Err(ApiError::Internal(error.to_string())),
    }
    let archived=state.store.archive_standalone_job(id,current_user.id).await.map_err(storage_error)?;
    state.pkuvideo_jobs.write().await.insert(id,archived);

    Ok(Json(serde_json::json!({
        "success": true,
        "message": "任务文件已删除，历史记录保留"
    })))
}

fn storage_error(error:Box<dyn std::error::Error+Send+Sync>)->ApiError {
    match error.to_string().as_str() {
        "STANDALONE_STORAGE_FULL"=>ApiError::Conflict("课堂笔记生成页最多保留 3 个任务，请先删除已有任务文件释放名额".into()),
        "STORAGE_QUOTA_EXCEEDED"=>ApiError::Forbidden("存储空间不足，请先删除已有任务文件".into()),
        "ACTIVE_JOB"=>ApiError::Conflict("排队或处理中的任务不能删除".into()),
        "COURSE_JOB"=>ApiError::BadRequest("课程笔记请在课程页面管理".into()),
        "OWNER_MISMATCH"=>ApiError::Forbidden("无权访问".into()),
        "JOB_NOT_FOUND"=>ApiError::NotFound,
        _=>ApiError::Internal(error.to_string()),
    }
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

    let job_id = Uuid::new_v4();
    let artifacts_dir = format!("./data/pkuvideo_jobs/{job_id}");

    let mut job = PkuVideoJob::new(job_id, user_id, media.url.clone(), artifacts_dir);
    job.regenerate = regenerate;
    // Arbitrary direct URLs may contain private media. They stay outside sharing.
    // Classroom discovery uses the authorized course submission path instead.
    state.store.reserve_standalone_job(job.clone()).await.map_err(storage_error)?;
    state.pkuvideo_jobs.write().await.insert(job_id, job);

    if let Err(error) = crate::services::video_jobs::enqueue_prepared_job(&state, job_id, user_id, media).await {
        let mut failed=state.pkuvideo_jobs.read().await.get(&job_id).cloned().ok_or(ApiError::NotFound)?;
        failed.status=PipelineStatus::Failed;failed.error=Some("提交队列失败".into());failed.finished_at=Some(chrono::Utc::now());
        state.store.update_pkuvideo_job(failed.clone()).await.map_err(|e|ApiError::Internal(e.to_string()))?;
        state.pkuvideo_jobs.write().await.insert(job_id,failed);
        if let Ok(archived)=state.store.archive_standalone_job(job_id,user_id).await {state.pkuvideo_jobs.write().await.insert(job_id,archived);}
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
    if job.artifacts_deleted_at.is_some() { return Err(ApiError::NotFound); }
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

#[cfg(test)]
mod history_tests {
    use super::*;
    use crate::{config::Settings,store::{FileStore,FileUser,FileUserSession}};
    use chrono::{Duration,Utc};
    use std::sync::Arc;
    #[tokio::test]
    async fn history_is_private_and_standalone_list_excludes_course_and_deleted() {
        let temp=tempdir::TempDir::new("task-history-api").unwrap();
        let settings:Settings=serde_json::from_value(serde_json::json!({
            "server":{"address":"127.0.0.1","port":0},
            "wechat":{"api_base_url":"","oauth_app_id":"","oauth_app_secret":"","redirect_uri":"","qr_bot_type":""},
            "security":{"session_ttl_hours":1},"cors":{"allowed_origins":[],"allow_credentials":false},
            "audioapi_paraformer":{},"llm_deepseek":{}
        })).unwrap();
        let store=Arc::new(FileStore::new(temp.path().into()).await.unwrap());
        let owner=Uuid::new_v4();let token=Uuid::new_v4();let stranger=Uuid::new_v4();
        store.insert_user(FileUser{id:owner,username:"owner".into(),email:"owner@example.invalid".into(),password_hash:String::new(),is_active:true,created_at:Utc::now(),updated_at:Utc::now(),storage_quota:3,processing_quota:10,processing_priority:5,iaaa_encrypted_password:None,iaaa_public_key:None}).await.unwrap();
        store.insert_session(FileUserSession{session_id:token,user_id:owner,expires_at:Utc::now()+Duration::hours(1),created_at:Utc::now(),user_agent:None,ip_address:None}).await.unwrap();
        let direct=Uuid::new_v4();let course=Uuid::new_v4();let deleted=Uuid::new_v4();let foreign=Uuid::new_v4();
        for (id,user,course_id,removed) in [(direct,owner,None,false),(course,owner,Some("course"),false),(deleted,owner,None,true),(foreign,stranger,None,false)] {
            let mut job=PkuVideoJob::new(id,user,"SECRET_SOURCE_URL".into(),"SECRET_FILE_PATH".into());
            job.course_id=course_id.map(String::from);job.status=PipelineStatus::Succeeded;
            if removed {job.artifacts_deleted_at=Some(Utc::now());}
            if id==direct {store.reserve_standalone_job(job).await.unwrap();}
            else {store.insert_pkuvideo_job(job).await.unwrap();}
        }
        let direct_file=store.user_tex_path(owner,direct);
        crate::store::atomic_tex(&direct_file,b"\\section{Private direct note}").unwrap();
        let state=AppState::new(settings,store.clone());
        *state.pkuvideo_jobs.write().await=store.get_all_pkuvideo_jobs().await;
        let app=crate::routes::ui::routes(state.clone()).merge(routes()).with_state(state);
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base=format!("http://{}",listener.local_addr().unwrap());
        let server=tokio::spawn(async move {axum::serve(listener,app).await.unwrap();});
        let client=reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
        assert_eq!(client.get(format!("{base}/task-history")).send().await.unwrap().status(),303);
        for path in ["/api/video-task-history","/api/pkuvideo/standalone-jobs"] {
            assert_eq!(client.get(format!("{base}{path}")).send().await.unwrap().status(),401);
        }
        let page=client.get(format!("{base}/task-history")).bearer_auth(token).send().await.unwrap();
        assert_eq!(page.status(),200);assert_eq!(page.headers()["cache-control"],"no-store");
        let html=page.text().await.unwrap();assert!(html.contains("使用加速券"));
        assert!(!html.contains("downloadPdf")&&!html.contains("deleteJob")&&!html.contains("/download-tex"));
        let history:Vec<serde_json::Value>=client.get(format!("{base}/api/video-task-history?user_id={stranger}")).bearer_auth(token).send().await.unwrap().json().await.unwrap();
        assert_eq!(history.len(),3);
        assert!(history.iter().any(|j|j["id"]==deleted.to_string()));
        assert!(history.iter().any(|j|j["id"]==course.to_string()));
        for record in &history {
            assert!(record.get("pdf_path").is_none()&&record.get("tex_url").is_none());
            assert!(!record.to_string().contains("SECRET_"));
        }
        let list:Vec<serde_json::Value>=client.get(format!("{base}/api/pkuvideo/standalone-jobs")).bearer_auth(token).send().await.unwrap().json().await.unwrap();
        assert_eq!(list.len(),1);assert_eq!(list[0]["id"],direct.to_string());
        // A cleanup error must preserve the stored item and its reserved slot.
        std::fs::remove_file(&direct_file).unwrap();std::fs::create_dir(&direct_file).unwrap();
        assert_eq!(client.delete(format!("{base}/api/pkuvideo/jobs/{direct}")).bearer_auth(token).send().await.unwrap().status(),500);
        assert_eq!(store.get_user(owner).await.unwrap().storage_quota,2);
        assert!(store.get_pkuvideo_job(direct).await.unwrap().artifacts_deleted_at.is_none());
        std::fs::remove_dir(&direct_file).unwrap();crate::store::atomic_tex(&direct_file,b"\\section{Private direct note}").unwrap();
        assert_eq!(client.delete(format!("{base}/api/pkuvideo/jobs/{course}")).bearer_auth(token).send().await.unwrap().status(),400);
        for _ in 0..2 {
            assert_eq!(client.delete(format!("{base}/api/pkuvideo/jobs/{direct}")).bearer_auth(token).send().await.unwrap().status(),200);
        }
        assert!(!direct_file.exists());
        assert_eq!(store.get_user(owner).await.unwrap().storage_quota,3);
        let list:Vec<serde_json::Value>=client.get(format!("{base}/api/pkuvideo/standalone-jobs")).bearer_auth(token).send().await.unwrap().json().await.unwrap();
        assert!(list.is_empty());
        let retained:Vec<serde_json::Value>=client.get(format!("{base}/api/video-task-history")).bearer_auth(token).send().await.unwrap().json().await.unwrap();
        assert_eq!(retained.len(),3);
        assert!(retained.iter().find(|j|j["id"]==direct.to_string()).unwrap()["artifacts_deleted_at"].is_string());
        assert_eq!(client.post(format!("{base}/api/video-task-history")).bearer_auth(token).send().await.unwrap().status(),405);
        assert_eq!(client.get(format!("{base}/api/pkuvideo/jobs/{deleted}/tex")).bearer_auth(token).send().await.unwrap().status(),404);
        server.abort();
    }
}

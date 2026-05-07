use axum::{
    extract::State,
    routing::{delete, get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

use crate::{
    app_state::AppState,
    auth::CurrentUser,
    error::ApiError,
    services::{course_parser, course_storage, lesson_storage, pkulogin},
};

const IAAA_PUBLIC_KEY_URL: &str = "https://iaaa.pku.edu.cn/iaaa/getPublicKey.do";

#[derive(Debug, Deserialize, Serialize)]
struct IaaaPublicKeyResponse {
    success: bool,
    key: Option<String>,
}

#[derive(Debug, Serialize)]
struct BindingStatusResponse {
    is_bound: bool,
    iaaa_public_key: Option<String>,
    key_expired: bool,
    current_iaaa_key: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BindRequest {
    username: String,
    encrypted_password: String,
    iaaa_public_key: String,
}

#[derive(Debug, Serialize)]
struct Course {
    id: String,
    name: String,
    term: String,
    teacher: Option<String>,
    video_count: Option<i32>,
}

#[derive(Debug, Serialize)]
struct Video {
    id: String,
    title: String,
    duration: Option<i32>,
}

#[derive(Debug, Deserialize)]
struct BatchSubmitRequest {
    course_id: String,
    video_ids: Vec<String>,
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/pkuvideofullclass/binding-status", get(get_binding_status))
        .route("/api/pkuvideofullclass/bind", post(bind))
        .route("/api/pkuvideofullclass/unbind", delete(unbind))
        .route("/api/pkuvideofullclass/current-iaaa-key", get(get_current_iaaa_key))
        .route("/api/pkuvideofullclass/courses", get(get_courses))
        .route("/api/pkuvideofullclass/courses/refresh", post(refresh_courses))
        .route("/api/pkuvideofullclass/courses/:course_id/videos", get(get_course_videos))
        .route("/api/pkuvideofullclass/courses/:course_id/videos/refresh", post(refresh_lessons))
        .route("/api/pkuvideofullclass/batch-submit", post(batch_submit))
}

async fn get_current_iaaa_key(
    _current_user: CurrentUser,
) -> Result<Json<IaaaPublicKeyResponse>, ApiError> {
    let client = reqwest::Client::new();
    let resp = client
        .get(IAAA_PUBLIC_KEY_URL)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| ApiError::Internal(format!("Failed to fetch IAAA public key: {}", e)))?;

    let data: IaaaPublicKeyResponse = resp
        .json()
        .await
        .map_err(|e| ApiError::Internal(format!("Failed to parse IAAA response: {}", e)))?;

    Ok(Json(data))
}

async fn get_binding_status(
    State(state): State<AppState>,
    current_user: CurrentUser,
) -> Result<Json<BindingStatusResponse>, ApiError> {
    let user = state
        .store
        .get_user(current_user.id)
        .await
        .ok_or(ApiError::Unauthorized)?;

    let is_bound = user.iaaa_encrypted_password.is_some() && user.iaaa_public_key.is_some();

    let mut key_expired = false;
    let mut current_iaaa_key: Option<String> = None;

    if is_bound {
        let client = reqwest::Client::new();
        match client
            .get(IAAA_PUBLIC_KEY_URL)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
        {
            Ok(resp) => {
                if let Ok(key_data) = resp.json::<IaaaPublicKeyResponse>().await {
                    current_iaaa_key = key_data.key;
                    if let Some(ref stored_key) = user.iaaa_public_key {
                        if let Some(ref current_key) = current_iaaa_key {
                            if stored_key != current_key {
                                key_expired = true;
                            }
                        }
                    }
                }
            }
            Err(_) => {
                key_expired = true;
            }
        }
    }

    Ok(Json(BindingStatusResponse {
        is_bound,
        iaaa_public_key: user.iaaa_public_key,
        key_expired,
        current_iaaa_key,
    }))
}

async fn bind(
    State(state): State<AppState>,
    current_user: CurrentUser,
    Json(req): Json<BindRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if req.username.is_empty() || req.encrypted_password.is_empty() || req.iaaa_public_key.is_empty() {
        return Err(ApiError::BadRequest("Missing required fields".to_string()));
    }

    tracing::info!("IAAA binding attempt for user {} with username {}", current_user.id, req.username);

    match pkulogin::login_iaaa_and_get_cookies(&req.username, &req.encrypted_password).await {
        Ok(login_result) => {
            tracing::info!("IAAA authentication succeeded for user {}, token obtained", req.username);

            let _ = std::fs::remove_file(&login_result.cookie_file);

            state
                .store
                .update_iaaa_binding(current_user.id, req.encrypted_password, req.iaaa_public_key)
                .await
                .map_err(|e| {
                    tracing::error!("Failed to save IAAA binding for user {}: {}", current_user.id, e);
                    ApiError::Internal(format!("Failed to save binding: {}", e))
                })?;

            tracing::info!("IAAA binding saved successfully for user {}", current_user.id);

            Ok(Json(serde_json::json!({
                "success": true,
                "message": "绑定成功"
            })))
        }
        Err(err) => {
            tracing::error!("IAAA authentication failed for user {}: {}", req.username, err);
            Err(ApiError::BadRequest(format!("绑定失败: {}", err)))
        }
    }
}

async fn unbind(
    State(state): State<AppState>,
    current_user: CurrentUser,
) -> Result<Json<serde_json::Value>, ApiError> {
    state
        .store
        .remove_iaaa_binding(current_user.id)
        .await
        .map_err(|e| ApiError::Internal(format!("Failed to remove binding: {}", e)))?;

    Ok(Json(serde_json::json!({
        "success": true,
        "message": "解绑成功"
    })))
}

async fn get_courses(
    State(state): State<AppState>,
    current_user: CurrentUser,
) -> Result<Json<serde_json::Value>, ApiError> {
    let user = state.store.get_user(current_user.id).await.ok_or_else(|| {
        tracing::error!("用户不存在: {}", current_user.id);
        ApiError::Internal("用户不存在".to_string())
    })?;

    let username = user.email.split('@').next().ok_or_else(|| {
        tracing::error!("邮箱格式无效: {}", user.email);
        ApiError::BadRequest("无效的邮箱格式".to_string())
    })?;

    let is_bound = user.iaaa_encrypted_password.is_some();

    let courses = course_storage::load_courses(username)
        .map_err(|e| {
            tracing::error!("加载课程信息失败: {}", e);
            ApiError::Internal(format!("加载课程信息失败: {}", e))
        })?;

    let last_updated = course_storage::get_last_updated(username);

    tracing::info!("用户 {} 加载了 {} 门课程", username, courses.len());

    Ok(Json(serde_json::json!({
        "success": true,
        "courses": courses,
        "is_bound": is_bound,
        "last_updated": last_updated,
        "message": if courses.is_empty() { "暂无课程信息，请刷新课程列表" } else { "" }
    })))
}

async fn get_course_videos(
    State(state): State<AppState>,
    axum::extract::Path((course_id,)): axum::extract::Path<(String,)>,
    current_user: CurrentUser,
) -> Result<Json<serde_json::Value>, ApiError> {
    let user = state.store.get_user(current_user.id).await.ok_or_else(|| {
        tracing::error!("用户不存在: {}", current_user.id);
        ApiError::Internal("用户不存在".to_string())
    })?;

    let username = user.email.split('@').next().ok_or_else(|| {
        tracing::error!("邮箱格式无效: {}", user.email);
        ApiError::BadRequest("无效的邮箱格式".to_string())
    })?;

    let lessons = lesson_storage::load_lessons(username, &course_id)
        .map_err(|e| {
            tracing::error!("加载课节信息失败: {}", e);
            ApiError::Internal(format!("加载课节信息失败: {}", e))
        })?;

    let (total, processed) = lesson_storage::get_lesson_count(username, &course_id);

    tracing::info!("用户 {} 课程 {} 加载了 {} 个课节", username, course_id, lessons.len());

    Ok(Json(serde_json::json!({
        "success": true,
        "lessons": lessons,
        "total": total,
        "processed": processed,
        "all_processed": total > 0 && processed == total
    })))
}

async fn refresh_lessons(
    State(state): State<AppState>,
    axum::extract::Path((course_id,)): axum::extract::Path<(String,)>,
    current_user: CurrentUser,
) -> Result<Json<serde_json::Value>, ApiError> {
    let user = state.store.get_user(current_user.id).await.ok_or_else(|| {
        tracing::error!("用户不存在: {}", current_user.id);
        ApiError::Internal("用户不存在".to_string())
    })?;

    let username = user.email.split('@').next().ok_or_else(|| {
        tracing::error!("邮箱格式无效: {}", user.email);
        ApiError::BadRequest("无效的邮箱格式".to_string())
    })?;

    let password = match user.iaaa_encrypted_password {
        Some(p) => p,
        None => {
            tracing::warn!("用户未绑定IAAA账号: {}", current_user.id);
            return Err(ApiError::BadRequest("未绑定IAAA账号，请先绑定".to_string()));
        }
    };

    tracing::info!("用户 {} 开始刷新课程 {} 的课节列表...", username, course_id);

    let login_result = pkulogin::login_iaaa_and_get_cookies_with_cache(&username, &password, &state.login_cache).await
        .map_err(|e| {
            tracing::error!("IAAA登录失败: {}", e);
            ApiError::BadRequest(format!("IAAA登录失败: {}", e))
        })?;

    let cookie_file_str = login_result.cookie_file.to_string_lossy().to_string();

    let course_url = format!("https://course.pku.edu.cn/webapps/blackboard/execute/launcher?type=Course&id=PkId{{{}}}", course_id);
    tracing::info!("开始访问课程页面: {}", course_url);

    let page_content = match pkulogin::fetch_with_cookie(&cookie_file_str, &login_result.user_agent, &course_url) {
        Ok(content) => content,
        Err(e) => {
            tracing::error!("获取课程页面失败: {}", e);
            state.login_cache.invalidate(username).await;
            return Err(ApiError::Internal(format!("获取课程页面失败: {}", e)));
        }
    };

    let mock_lessons = lesson_storage::LessonInfo {
        id: "1".to_string(),
        video_url: "http://example.com/video1.mp4".to_string(),
        date: "2025-03-01".to_string(),
        time: "09:00".to_string(),
        day_of_week: "周六".to_string(),
        is_processed: false,
        tex_filename: None,
    };

    lesson_storage::init_course_dir(username, &course_id)
        .map_err(|e| ApiError::Internal(format!("创建课节目录失败: {}", e)))?;
    lesson_storage::save_lessons(username, &course_id, &[mock_lessons])
        .map_err(|e| {
            tracing::error!("保存课节信息失败: {}", e);
            ApiError::Internal(format!("保存课节信息失败: {}", e))
        })?;

    tracing::info!("用户 {} 课程 {} 课节列表已保存", username, course_id);

    Ok(Json(serde_json::json!({
        "success": true,
        "message": "课节列表已刷新"
    })))
}

async fn refresh_courses(
    State(state): State<AppState>,
    current_user: CurrentUser,
) -> Result<Json<serde_json::Value>, ApiError> {
    let user = state.store.get_user(current_user.id).await.ok_or_else(|| {
        tracing::error!("用户不存在: {}", current_user.id);
        ApiError::Internal("用户不存在".to_string())
    })?;

    let username = user.email.split('@').next().ok_or_else(|| {
        tracing::error!("邮箱格式无效: {}", user.email);
        ApiError::BadRequest("无效的邮箱格式".to_string())
    })?;

    let password = match user.iaaa_encrypted_password {
        Some(p) => p,
        None => {
            tracing::warn!("用户未绑定IAAA账号: {}", current_user.id);
            return Err(ApiError::BadRequest("未绑定IAAA账号，请先绑定".to_string()));
        }
    };

    tracing::info!("用户 {} 开始刷新课程列表...", username);

    let login_result = pkulogin::login_iaaa_and_get_cookies_with_cache(&username, &password, &state.login_cache).await
        .map_err(|e| {
            tracing::error!("IAAA登录失败: {}", e);
            ApiError::BadRequest(format!("IAAA登录失败: {}", e))
        })?;

    let cookie_file_str = login_result.cookie_file.to_string_lossy().to_string();
    tracing::info!("登录成功，Cookie文件: {}", cookie_file_str);

    let portal_url = "https://course.pku.edu.cn/webapps/portal/execute/tabs/tabAction?tab_tab_group_id=_1_1";
    tracing::info!("开始访问教学网首页: {}", portal_url);

    let page_content = match pkulogin::fetch_with_cookie(&cookie_file_str, &login_result.user_agent, portal_url) {
        Ok(content) => content,
        Err(e) => {
            tracing::error!("获取教学网页面失败: {}", e);
            state.login_cache.invalidate(username).await;
            return Err(ApiError::Internal(format!("获取教学网页面失败: {}", e)));
        }
    };

    let courses = course_parser::parse_courses(&page_content);
    tracing::info!("解析出 {} 门课程", courses.len());

    course_storage::save_courses(username, &courses)
        .map_err(|e| {
            tracing::error!("保存课程信息失败: {}", e);
            ApiError::Internal(format!("保存课程信息失败: {}", e))
        })?;

    Ok(Json(serde_json::json!({
        "success": true,
        "courses": courses,
        "count": courses.len(),
        "message": format!("成功刷新 {} 门课程", courses.len())
    })))
}

async fn batch_submit(
    State(_state): State<AppState>,
    _current_user: CurrentUser,
    Json(_req): Json<BatchSubmitRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    Ok(Json(serde_json::json!({
        "success": true,
        "job_id": "mock-job-id"
    })))
}

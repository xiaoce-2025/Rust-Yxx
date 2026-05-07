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
    services::{course_parser, course_storage, lesson_parser, lesson_storage, pkulogin, video_downloader},
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
        .route("/api/pkuvideofullclass/courses/:course_id/videos/:video_id/process", post(process_lesson))
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

    let mut all_lessons = Vec::new();
    let mut current_url = format!("https://course.pku.edu.cn/webapps/bb-streammedia-hqy-BBLEARN/videoList.action?course_id={}&mode=view", course_id);
    let mut page_num = 1;

    loop {
        tracing::info!("开始访问视频列表页面 {}: {}", page_num, current_url);

        let page_content = match pkulogin::fetch_with_cookie(&cookie_file_str, &login_result.user_agent, &current_url) {
            Ok(content) => content,
            Err(e) => {
                tracing::error!("获取视频列表页面失败: {}", e);
                state.login_cache.invalidate(username).await;
                return Err(ApiError::Internal(format!("获取视频列表页面失败: {}", e)));
            }
        };

        let mut lessons = lesson_parser::parse_lessons(&page_content, &course_id);
        all_lessons.append(&mut lessons);

        if !lesson_parser::has_next_page(&page_content) {
            tracing::info!("已到达最后一页，共获取 {} 个课节", all_lessons.len());
            break;
        }

        match lesson_parser::get_next_page_url(&page_content) {
            Some(next_url) => {
                current_url = next_url;
                page_num += 1;
            }
            None => {
                tracing::warn!("检测到有下一页，但无法获取下一页URL");
                break;
            }
        }
    }

    if all_lessons.is_empty() {
        tracing::warn!("课程 {} 未解析到任何课节", course_id);
    }

    lesson_parser::assign_lesson_numbers(&mut all_lessons);

    lesson_storage::init_course_dir(username, &course_id)
        .map_err(|e| ApiError::Internal(format!("创建课节目录失败: {}", e)))?;
    lesson_storage::save_lessons(username, &course_id, &all_lessons)
        .map_err(|e| {
            tracing::error!("保存课节信息失败: {}", e);
            ApiError::Internal(format!("保存课节信息失败: {}", e))
        })?;

    tracing::info!("用户 {} 课程 {} 课节列表已保存，共 {} 个课节", username, course_id, all_lessons.len());

    Ok(Json(serde_json::json!({
        "success": true,
        "message": format!("成功刷新 {} 个课节", all_lessons.len()),
        "count": all_lessons.len()
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

async fn process_lesson(
    State(state): State<AppState>,
    axum::extract::Path((course_id, video_id)): axum::extract::Path<(String, String)>,
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

    let mut lessons = lesson_storage::load_lessons(username, &course_id)
        .map_err(|e| {
            tracing::error!("加载课节信息失败: {}", e);
            ApiError::Internal(format!("加载课节信息失败: {}", e))
        })?;

    let lesson_index = lessons.iter().position(|l| l.id == video_id);

    if lesson_index.is_none() {
        return Err(ApiError::BadRequest("课节不存在".to_string()));
    }

    let lesson_index = lesson_index.unwrap();
    let mut is_newly_processed = false;
    
    if lessons[lesson_index].video_url.is_empty() {
        tracing::info!("课节 {} 下载链接为空，开始获取播放链接并刷新课节列表", video_id);

        let login_result = pkulogin::login_iaaa_and_get_cookies_with_cache(&username, &password, &state.login_cache).await
            .map_err(|e| {
                tracing::error!("IAAA登录失败: {}", e);
                ApiError::BadRequest(format!("IAAA登录失败: {}", e))
            })?;

        let cookie_file_str = login_result.cookie_file.to_string_lossy().to_string();

        if !lessons[lesson_index].play_url.is_empty() {
            tracing::info!("尝试获取下载链接，播放链接: {}", lessons[lesson_index].play_url);
            
            match video_downloader::get_video_download_url(&lessons[lesson_index].play_url, &cookie_file_str, &login_result.user_agent).await {
                Ok(download_url) => {
                    lessons[lesson_index].video_url = download_url;
                    lessons[lesson_index].is_processed = true;
                    is_newly_processed = true;
                    tracing::info!("成功获取下载链接: {}", lessons[lesson_index].video_url);
                }
                Err(e) => {
                    tracing::warn!("获取下载链接失败: {}", e);
                    return Err(ApiError::Internal(format!("获取下载链接失败: {}", e)));
                }
            }
        } else {
            tracing::warn!("课节 {} 播放链接为空，无法获取下载链接", video_id);
            return Err(ApiError::BadRequest("播放链接为空，请先刷新课节列表".to_string()));
        }

        lesson_storage::save_lessons(username, &course_id, &lessons)
            .map_err(|e| {
                tracing::error!("保存课节信息失败: {}", e);
                ApiError::Internal(format!("保存课节信息失败: {}", e))
            })?;
    } else {
        tracing::info!("课节 {} 已有下载链接，跳过获取", video_id);
    }

    let lesson = &lessons[lesson_index];
    let message = if is_newly_processed {
        "下载链接已获取"
    } else if !lesson.video_url.is_empty() {
        "下载链接已存在"
    } else {
        "下载链接获取失败"
    };
    
    Ok(Json(serde_json::json!({
        "success": true,
        "message": message,
        "video_id": video_id,
        "video_url": lesson.video_url,
        "play_url": lesson.play_url,
        "is_processed": lesson.is_processed
    })))
}

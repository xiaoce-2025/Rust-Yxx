use axum::{
    extract::State,
    response::IntoResponse,
    routing::{delete, get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

use crate::{
    app_state::AppState,
    auth::CurrentUser,
    error::ApiError,
    routes::pkuvideo::run_job_pipeline,
    services::{
        course_parser, course_storage, lesson_parser, lesson_storage, pkulogin, pkuvideo::{PipelineStatus, PkuVideoJob}, 
        video_downloader
    },
};
use lesson_storage::LessonInfo;

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
        .route("/api/pkuvideofullclass/courses/:course_id/videos/:video_id/download-pdf", get(download_lesson_pdf))
        .route("/api/pkuvideofullclass/courses/:course_id/download-pdf", get(download_course_pdf))
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

    // 先加载已有的课节数据
    let existing_lessons = match lesson_storage::load_lessons(username, &course_id) {
        Ok(lessons) => {
            tracing::info!("已加载 {} 个现有课节", lessons.len());
            lessons
        }
        Err(_) => {
            tracing::info!("未找到现有课节数据");
            Vec::new()
        }
    };

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

    // 合并已有的课节数据：保留 video_url, is_processed, tex_filename，只更新 play_url
    merge_lesson_data(&mut all_lessons, &existing_lessons);

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

fn merge_lesson_data(new_lessons: &mut Vec<LessonInfo>, existing_lessons: &Vec<LessonInfo>) {
    for new_lesson in new_lessons {
        // 查找日期和时间相同的已有课节
        if let Some(existing) = existing_lessons.iter().find(|l| l.date == new_lesson.date && l.time == new_lesson.time) {
            tracing::info!("找到匹配的课节: {} {}，保留原有数据", new_lesson.date, new_lesson.time);
            // 保留原有数据，只更新播放链接
            new_lesson.video_url = existing.video_url.clone();
            new_lesson.is_processed = existing.is_processed;
            new_lesson.tex_filename = existing.tex_filename.clone();
            // play_url 从新解析结果中获取（可能已更新）
        }
    }
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
    
    // 步骤1: 获取下载链接（如果为空）
    if lessons[lesson_index].video_url.is_empty() {
        tracing::info!("课节 {} 下载链接为空，开始获取下载链接", video_id);

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

    // 保存需要返回的视频信息
    let video_url = lessons[lesson_index].video_url.clone();
    let play_url = lessons[lesson_index].play_url.clone();
    
    // 步骤2: 提交pkuvideo任务（不扣除限额）
    if video_url.is_empty() {
        return Err(ApiError::Internal("下载链接为空，无法提交任务".to_string()));
    }

    tracing::info!("提交pkuvideo任务，视频URL: {}", video_url);
    
    let job_id = enqueue_job_without_limit(
        &state,
        current_user.id,
        &video_url,
        &username,
        &password,
    ).await?;

    tracing::info!("任务提交成功，job_id: {}", job_id);

    // 步骤3: 等待任务完成
    tracing::info!("等待任务 {} 完成...", job_id);
    let job_result = wait_for_job_completion(&state, job_id).await?;

    if !job_result.success {
        return Err(ApiError::Internal(format!("任务执行失败: {}", job_result.message)));
    }

    tracing::info!("任务 {} 执行完成", job_id);

    // 步骤4: 复制.tex文件到course_data目录
    let tex_filename = format!("{}_lesson_{}.tex", course_id, video_id);
    let course_data_dir = format!("./course_data/{}/{}", username, course_id);
    
    if let Some(notes_path) = &job_result.notes_path {
        // notes_path是相对于./data/pkuvideo_jobs/的相对路径，需要转换为完整路径
        let base_dir = PathBuf::from("./data/pkuvideo_jobs");
        let src_path = base_dir.join(notes_path);
        
        tracing::info!("尝试复制tex文件: {} -> {}", src_path.display(), course_data_dir);
        
        if !src_path.exists() {
            tracing::warn!("源tex文件不存在: {}", src_path.display());
        } else {
            let dest_dir = PathBuf::from(&course_data_dir);
            
            // 确保目标目录存在
            std::fs::create_dir_all(&dest_dir).map_err(|e| ApiError::Internal(format!("创建目录失败: {}", e)))?;
            
            let dest_path = dest_dir.join(&tex_filename);
            std::fs::copy(&src_path, &dest_path).map_err(|e| ApiError::Internal(format!("复制文件失败: {}", e)))?;
            
            tracing::info!("tex文件已复制到: {}", dest_path.display());
            
            // 更新课节的tex文件名
            lessons[lesson_index].tex_filename = Some(tex_filename.clone());
            lessons[lesson_index].is_processed = true;
            lesson_storage::save_lessons(username, &course_id, &lessons)
                .map_err(|e| ApiError::Internal(format!("保存课节信息失败: {}", e)))?;
        }
    } else {
        tracing::warn!("任务完成但未生成tex文件");
    }

    // 步骤5: 删除pkuvideo对应的资源
    if let Some(artifacts_dir) = &job_result.artifacts_dir {
        match std::fs::remove_dir_all(artifacts_dir) {
            Ok(_) => tracing::info!("已清理pkuvideo任务资源: {}", artifacts_dir),
            Err(e) => tracing::warn!("清理pkuvideo任务资源失败: {}", e),
        }
    }

    // 步骤6: 删除内存中的任务记录
    {
        let mut jobs = state.pkuvideo_jobs.write().await;
        jobs.remove(&job_id);
    }
    state.store.delete_pkuvideo_job(job_id).await.map_err(|e| ApiError::Internal(format!("删除任务记录失败: {}", e)))?;

    let message = if is_newly_processed {
        "下载链接已获取并处理完成"
    } else {
        "处理完成"
    };
    
    Ok(Json(serde_json::json!({
        "success": true,
        "message": message,
        "video_id": video_id,
        "video_url": video_url,
        "play_url": play_url,
        "is_processed": lessons[lesson_index].is_processed,
        "tex_filename": lessons[lesson_index].tex_filename,
        "job_id": job_id.to_string()
    })))
}

async fn enqueue_job_without_limit(
    state: &AppState,
    user_id: Uuid,
    video_url: &str,
    pku_auth_one: &str,
    pku_auth_two: &str,
) -> Result<Uuid, ApiError> {
    let job_id = Uuid::new_v4();
    let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string();
    let artifacts_dir = format!("./data/pkuvideo_jobs/{}_{}", timestamp, job_id);
    
    let job = PkuVideoJob::new(job_id, user_id, video_url.to_string(), artifacts_dir.clone());
    
    // 保存到内存和文件存储
    state.pkuvideo_jobs.write().await.insert(job_id, job.clone());
    state.store.insert_pkuvideo_job(job).await.map_err(|e| ApiError::Internal(e.to_string()))?;

    // 异步执行任务（不扣除限额）
    let state_clone = state.clone();
    let video_url_clone = video_url.to_string();
    let pku_auth_one_clone = pku_auth_one.to_string();
    let pku_auth_two_clone = pku_auth_two.to_string();
    
    tokio::spawn(async move {
        let _ = run_job_pipeline(state_clone, job_id, video_url_clone, pku_auth_one_clone, pku_auth_two_clone).await;
    });

    Ok(job_id)
}

struct JobResult {
    success: bool,
    message: String,
    notes_path: Option<String>,
    artifacts_dir: Option<String>,
}

async fn wait_for_job_completion(state: &AppState, job_id: Uuid) -> Result<JobResult, ApiError> {
    let max_wait_seconds = 3600; // 最多等待1小时
    let check_interval_ms = 3000; // 每3秒检查一次
    
    for _ in 0..(max_wait_seconds * 1000 / check_interval_ms) {
        {
            let jobs = state.pkuvideo_jobs.read().await;
            if let Some(job) = jobs.get(&job_id) {
                match job.status {
                    PipelineStatus::Succeeded => {
                        return Ok(JobResult {
                            success: true,
                            message: "任务成功完成".to_string(),
                            notes_path: job.notes_path.clone(),
                            artifacts_dir: Some(job.artifacts_dir.clone()),
                        });
                    }
                    PipelineStatus::Failed => {
                        let error_msg = job.error.clone().unwrap_or_else(|| "未知错误".to_string());
                        return Ok(JobResult {
                            success: false,
                            message: error_msg,
                            notes_path: None,
                            artifacts_dir: Some(job.artifacts_dir.clone()),
                        });
                    }
                    _ => {
                        // 任务仍在执行，继续等待
                    }
                }
            }
        }
        
        tokio::time::sleep(tokio::time::Duration::from_millis(check_interval_ms)).await;
    }
    
    Err(ApiError::Internal("任务执行超时".to_string()))
}

async fn download_lesson_pdf(
    State(state): State<AppState>,
    axum::extract::Path((course_id, video_id)): axum::extract::Path<(String, String)>,
    current_user: CurrentUser,
) -> Result<impl IntoResponse, ApiError> {
    let user = state.store.get_user(current_user.id).await.ok_or_else(|| {
        tracing::error!("用户不存在: {}", current_user.id);
        ApiError::Internal("用户不存在".to_string())
    })?;

    let username = user.email.split('@').next().ok_or_else(|| {
        tracing::error!("邮箱格式无效: {}", user.email);
        ApiError::BadRequest("无效的邮箱格式".to_string())
    })?;

    // 加载课节信息
    let lessons = lesson_storage::load_lessons(username, &course_id)
        .map_err(|e| {
            tracing::error!("加载课节信息失败: {}", e);
            ApiError::Internal(format!("加载课节信息失败: {}", e))
        })?;

    // 找到对应的课节
    let lesson = lessons.iter().find(|l| l.id == video_id)
        .ok_or_else(|| ApiError::BadRequest("课节不存在".to_string()))?;

    // 检查是否存在tex文件
    let tex_filename = match &lesson.tex_filename {
        Some(f) => f,
        None => return Err(ApiError::BadRequest("该课节尚未生成tex文件".to_string())),
    };

    // 构建tex文件路径
    let tex_path = PathBuf::from(format!("./course_data/{}/{}/{}", username, course_id, tex_filename));
    
    if !tex_path.exists() {
        return Err(ApiError::Internal("tex文件不存在".to_string()));
    }

    // 创建临时目录用于编译
    let temp_dir = tempdir::TempDir::new("pkuvideo_pdf")
        .map_err(|e| ApiError::Internal(format!("创建临时目录失败: {}", e)))?;
    
    let temp_dir_path = temp_dir.path();
    tracing::info!("创建临时编译目录: {}", temp_dir_path.display());

    // 复制tex文件到临时目录
    let temp_tex_path = temp_dir_path.join(tex_filename);
    std::fs::copy(&tex_path, &temp_tex_path)
        .map_err(|e| ApiError::Internal(format!("复制tex文件失败: {}", e)))?;

    // 复制模板文件到临时目录
    let template_cls = PathBuf::from("./data/templates/elegantbook/elegantbook.cls");
    if !template_cls.exists() {
        return Err(ApiError::Internal("模板文件不存在".to_string()));
    }
    std::fs::copy(&template_cls, temp_dir_path.join("elegantbook.cls"))
        .map_err(|e| ApiError::Internal(format!("复制模板文件失败: {}", e)))?;

    // 读取tex内容并创建完整的LaTeX文档
    let chapter_content = std::fs::read_to_string(&temp_tex_path)
        .map_err(|e| ApiError::Internal(format!("读取tex文件失败: {}", e)))?;

    let full_doc = format!("\\documentclass[lang=cn,10pt,scheme=chinese,a4paper]{{elegantbook}}\n\n\\title{{课堂讲义}}\n\\author{{自动生成}}\n\\date{{}}\n\n\\begin{{document}}\n\n\\mainmatter\n\n{}\n\n\\end{{document}}\n", chapter_content);

    // 写入完整文档
    let full_tex_path = temp_dir_path.join("full_document.tex");
    std::fs::write(&full_tex_path, full_doc)
        .map_err(|e| ApiError::Internal(format!("写入完整文档失败: {}", e)))?;

    // 编译PDF - xelatex需要多次运行才能完成交叉引用和目录生成
    let pdf_path = temp_dir_path.join("full_document.pdf");
    
    // 将路径转换为使用正斜杠，确保跨平台兼容性
    let full_tex_path_str = {
        let s = full_tex_path.to_string_lossy();
        let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
        s.replace('\\', "/")
    };
    
    let temp_dir_str = {
        let s = temp_dir_path.to_string_lossy();
        let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
        s.replace('\\', "/")
    };
    
    for pass in 1..=3 {
        tracing::info!("xelatex编译第{}次...", pass);
        
        let result = tokio::process::Command::new("xelatex")
            .arg("-interaction=nonstopmode")
            .arg("-output-directory")
            .arg(&temp_dir_str)
            .arg(&full_tex_path_str)
            .current_dir(temp_dir_path)
            .output()
            .await;

        match result {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let exit_code = output.status.code();
                
                tracing::info!("xelatex第{}次编译完成 - exit_code: {:?}", pass, exit_code);
                tracing::debug!("xelatex stdout: {}", stdout);
                tracing::debug!("xelatex stderr: {}", stderr);
                
                // 检查PDF是否已生成（即使exit code非0也可能生成了PDF）
                if pdf_path.exists() {
                    tracing::info!("xelatex第{}次编译后PDF已生成: {:?}", pass, pdf_path);
                } else {
                    tracing::warn!("xelatex第{}次编译后PDF尚未生成", pass);
                    
                    // 如果exit code表示真正失败（非0且不是因警告），才返回错误
                    if !output.status.success() {
                        tracing::error!("xelatex第{}次编译失败 - exit_code: {:?}, stdout: {}, stderr: {}", 
                            pass, exit_code, stdout, stderr);
                        return Err(ApiError::Internal(format!("PDF编译失败 (第{}次): exit_code={:?}, {}", pass, exit_code, stderr)));
                    }
                }
            }
            Err(e) => {
                tracing::error!("执行xelatex失败: {}", e);
                return Err(ApiError::Internal(format!("执行xelatex失败: {}", e)));
            }
        }
    }

    // 检查PDF是否生成成功
    if !pdf_path.exists() {
        return Err(ApiError::Internal("PDF生成失败，文件不存在".to_string()));
    }

    // 使用tokio::fs读取PDF内容
    let pdf_data = tokio::fs::read(&pdf_path)
        .await
        .map_err(|e| ApiError::Internal(format!("读取PDF文件失败: {}", e)))?;

    // 构建文件名
    let pdf_filename = format!("{}.pdf", tex_filename.replace(".tex", ""));

    // 构建响应
    let response = axum::response::Response::builder()
        .header("Content-Type", "application/pdf")
        .header("Content-Disposition", format!("attachment; filename=\"{}\"", pdf_filename))
        .header("Content-Length", pdf_data.len())
        .body(axum::body::Body::from(pdf_data))
        .map_err(|e| ApiError::Internal(format!("构建响应失败: {}", e)))?;

    // 临时目录会在函数结束时自动删除
    Ok(response)
}

async fn download_course_pdf(
    State(state): State<AppState>,
    axum::extract::Path(course_id): axum::extract::Path<String>,
    current_user: CurrentUser,
) -> Result<impl IntoResponse, ApiError> {
    let user = state.store.get_user(current_user.id).await.ok_or_else(|| {
        tracing::error!("用户不存在: {}", current_user.id);
        ApiError::Internal("用户不存在".to_string())
    })?;

    let username = user.email.split('@').next().ok_or_else(|| {
        tracing::error!("邮箱格式无效: {}", user.email);
        ApiError::BadRequest("无效的邮箱格式".to_string())
    })?;

    // 加载课节信息
    let lessons = lesson_storage::load_lessons(username, &course_id)
        .map_err(|e| {
            tracing::error!("加载课节信息失败: {}", e);
            ApiError::Internal(format!("加载课节信息失败: {}", e))
        })?;

    // 筛选已处理的课节（有tex文件）
    let mut processed_lessons: Vec<&LessonInfo> = lessons.iter()
        .filter(|l| l.tex_filename.is_some())
        .collect();

    if processed_lessons.is_empty() {
        return Err(ApiError::BadRequest("没有已处理的课节".to_string()));
    }

    // 按时间先后排序（日期+时间）
    processed_lessons.sort_by(|a, b| {
        let a_datetime = format!("{} {}", a.date, a.time);
        let b_datetime = format!("{} {}", b.date, b.time);
        a_datetime.cmp(&b_datetime)
    });

    // 获取课程信息
    let courses = course_storage::load_courses(username)
        .map_err(|e| {
            tracing::error!("加载课程信息失败: {}", e);
            ApiError::Internal(format!("加载课程信息失败: {}", e))
        })?;

    let course_name = courses.iter()
        .find(|c| c.key == course_id)
        .map(|c| c.name.clone())
        .unwrap_or_else(|| "未知课程".to_string());

    // 创建临时目录
    let temp_dir = tempdir::TempDir::new("pkuvideo_course_pdf").map_err(|e| {
        tracing::error!("创建临时目录失败: {}", e);
        ApiError::Internal(format!("创建临时目录失败: {}", e))
    })?;
    let temp_dir_path = temp_dir.path();
    tracing::info!("创建临时编译目录: {:?}", temp_dir_path);

    // 复制elegantbook模板文件到临时目录
    let template_cls = std::path::Path::new("./data/templates/elegantbook/elegantbook.cls");
    let template_cls_abs = template_cls.canonicalize().map_err(|e| {
        tracing::error!("无法获取模板文件绝对路径: {}", e);
        ApiError::Internal(format!("无法获取模板文件绝对路径: {}", e))
    })?;

    if !template_cls_abs.exists() {
        return Err(ApiError::Internal(format!("模板文件不存在: {:?}", template_cls_abs)));
    }

    tokio::fs::copy(&template_cls_abs, temp_dir_path.join("elegantbook.cls")).await.map_err(|e| {
        tracing::error!("复制模板文件失败: {}", e);
        ApiError::Internal(format!("复制模板文件失败: {}", e))
    })?;
    tracing::info!("已复制elegantbook模板文件");

    // 构建完整的LaTeX文档内容
    let mut full_content = String::new();
    
    // 封面和目录部分 - 使用elegantbook模板
    full_content.push_str(r#"\documentclass[lang=cn,10pt,scheme=chinese,a4paper]{elegantbook}

\title{"#);
    full_content.push_str(&course_name);
    full_content.push_str(r#"}
\author{严小希}
\date{}

\begin{document}

\mainmatter

\tableofcontents

"#);

    // 添加各个课节内容
    for lesson in processed_lessons.iter() {
        if let Some(tex_filename) = &lesson.tex_filename {
            let tex_path = PathBuf::from("./course_data")
                .join(username)
                .join(&course_id)
                .join(tex_filename);
            
            match tokio::fs::read_to_string(&tex_path).await {
                Ok(content) => {
                    // 在\chapter{标题}的末尾添加日期
                    let modified_content = if let Some(chapter_start) = content.find("\\chapter{") {
                        let after_chapter = chapter_start + 9; // 跳过 "\\chapter{"
                        if let Some(close_brace) = content[after_chapter..].find("}") {
                            let insert_pos = after_chapter + close_brace;
                            let mut result = content.clone();
                            result.insert_str(insert_pos, &format!(" ({})", lesson.date));
                            result
                        } else {
                            content
                        }
                    } else {
                        content
                    };
                    full_content.push_str(&modified_content);
                    full_content.push_str("\n");
                }
                Err(e) => {
                    tracing::warn!("读取课节tex文件失败: {} - {}", tex_path.display(), e);
                }
            }
        }
    }

    // 文档结束
    full_content.push_str("\n\\end{document}\n");

    // 将完整内容写入临时文件
    let full_tex_path = temp_dir_path.join("full_document.tex");
    tokio::fs::write(&full_tex_path, &full_content).await.map_err(|e| {
        tracing::error!("写入tex文件失败: {}", e);
        ApiError::Internal(format!("写入tex文件失败: {}", e))
    })?;

    // 编译PDF - xelatex需要多次运行才能完成交叉引用和目录生成
    let pdf_path = temp_dir_path.join("full_document.pdf");
    
    // 将路径转换为使用正斜杠，确保跨平台兼容性
    let full_tex_path_str = {
        let s = full_tex_path.to_string_lossy();
        let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
        s.replace('\\', "/")
    };
    
    let temp_dir_str = {
        let s = temp_dir_path.to_string_lossy();
        let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
        s.replace('\\', "/")
    };
    
    for pass in 1..=3 {
        tracing::info!("xelatex编译第{}次...", pass);
        
        let result = tokio::process::Command::new("xelatex")
            .arg("-interaction=nonstopmode")
            .arg("-output-directory")
            .arg(&temp_dir_str)
            .arg(&full_tex_path_str)
            .current_dir(temp_dir_path)
            .output()
            .await;

        match result {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let exit_code = output.status.code();
                
                tracing::info!("xelatex第{}次编译完成 - exit_code: {:?}", pass, exit_code);
                
                // 检查PDF是否已生成（即使exit code非0也可能生成了PDF）
                if pdf_path.exists() {
                    tracing::info!("xelatex第{}次编译后PDF已生成: {:?}", pass, pdf_path);
                } else {
                    tracing::warn!("xelatex第{}次编译后PDF尚未生成", pass);
                    
                    // 如果exit code表示真正失败（非0且不是因警告），才返回错误
                    if !output.status.success() {
                        tracing::error!("xelatex第{}次编译失败 - exit_code: {:?}, stdout: {}, stderr: {}", 
                            pass, exit_code, stdout, stderr);
                        return Err(ApiError::Internal(format!("PDF编译失败 (第{}次): exit_code={:?}, {}", pass, exit_code, stderr)));
                    }
                }
            }
            Err(e) => {
                tracing::error!("执行xelatex失败: {}", e);
                return Err(ApiError::Internal(format!("执行xelatex失败: {}", e)));
            }
        }
    }

    // 检查PDF是否生成成功
    if !pdf_path.exists() {
        return Err(ApiError::Internal("PDF生成失败，文件不存在".to_string()));
    }

    // 使用tokio::fs读取PDF内容
    let pdf_data = tokio::fs::read(&pdf_path)
        .await
        .map_err(|e| ApiError::Internal(format!("读取PDF文件失败: {}", e)))?;

    // 构建文件名
    let pdf_filename = format!("{}_{}.pdf", course_id, course_name.replace(|c: char| !c.is_alphanumeric(), "_"));

    // 构建响应
    let response = axum::response::Response::builder()
        .header("Content-Type", "application/pdf")
        .header("Content-Disposition", format!("attachment; filename=\"{}\"", pdf_filename))
        .header("Content-Length", pdf_data.len())
        .body(axum::body::Body::from(pdf_data))
        .map_err(|e| ApiError::Internal(format!("构建响应失败: {}", e)))?;

    // 临时目录会在函数结束时自动删除
    Ok(response)
}

use axum::{
    extract::State,
    response::IntoResponse,
    routing::{delete, get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;
use uuid::Uuid;

use crate::{
    app_state::AppState,
    auth::CurrentUser,
    error::ApiError,
    services::{
        course_parser, course_storage, lesson_parser, lesson_storage, pkulogin, pkuvideo::PipelineStatus
    },
};
use lesson_storage::LessonInfo;

const IAAA_PUBLIC_KEY_URL: &str = "https://iaaa.pku.edu.cn/iaaa/getPublicKey.do";
const MAX_VIDEO_LIST_PAGES: usize = 100;

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

#[derive(Deserialize)]
struct BindRequest {
    password: Option<String>,
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
    #[serde(default)]
    video_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ProcessLessonRequest {
    manual_key: Option<String>,
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
        .route("/api/pkuvideofullclass/batch-submit/status", get(batch_submit_status))
        .route("/api/pkuvideofullclass/courses/:course_id/videos/:video_id/process", post(process_lesson))
        .route("/api/pkuvideofullclass/courses/:course_id/videos/:video_id/reprocess", post(reprocess_lesson))
        .route("/api/pkuvideofullclass/courses/:course_id/videos/:video_id/process-status", get(get_process_status))
        .route("/api/pkuvideofullclass/courses/:course_id/videos/:video_id/download-pdf", get(download_lesson_pdf))
        .route("/api/pkuvideofullclass/courses/:course_id/videos/:video_id/download-tex", get(download_lesson_tex))
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
    Json(mut req): Json<BindRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if let Some(password) = req.password.as_ref() {
        let response = reqwest::Client::new().get(IAAA_PUBLIC_KEY_URL)
            .timeout(std::time::Duration::from_secs(10)).send().await?;
        let key: IaaaPublicKeyResponse = response.error_for_status()?.json().await?;
        req.iaaa_public_key = key.key.filter(|_|key.success).ok_or_else(||ApiError::BadRequest("无法获取当前 IAAA 公钥".into()))?;
        req.encrypted_password = crate::services::app_replay::encrypt_binding_password(&req.iaaa_public_key, password).map_err(ApiError::BadRequest)?;
    }
    if req.username.is_empty() || req.encrypted_password.is_empty() || req.iaaa_public_key.is_empty() {
        return Err(ApiError::BadRequest("Missing required fields".to_string()));
    }

    tracing::info!("IAAA binding attempt for user {} with username {}", current_user.id, req.username);

    match pkulogin::login_iaaa_and_get_cookies(&req.username, &req.encrypted_password).await {
        Ok(login_result) => {
            tracing::info!("IAAA authentication succeeded for user {}, token obtained", req.username);

            drop(login_result);
            let username = current_user.email.split('@').next().unwrap_or_default();
            let login_lock = state.login_cache.login_lock(username).await;
            let _login_guard = login_lock.lock().await;
            state.login_cache.invalidate(username).await;

            crate::services::app_replay::clear_binding(current_user.id).map_err(ApiError::Internal)?;
            if let Some(password) = req.password.as_ref() {
                crate::services::app_replay::save_binding(current_user.id, &req.username, password).map_err(ApiError::Internal)?;
            }
            state.app_replay_clients.lock().await.remove(&current_user.id);

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
    let username = current_user.email.split('@').next().unwrap_or_default();
    let login_lock = state.login_cache.login_lock(username).await;
    let _login_guard = login_lock.lock().await;
    state.login_cache.invalidate(username).await;
    crate::services::app_replay::clear_binding(current_user.id).map_err(ApiError::Internal)?;
    state.app_replay_clients.lock().await.remove(&current_user.id);
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
    let lesson_mutex = state.lesson_mutation(current_user.id).await;
    let _guard = lesson_mutex.lock().await;
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
    let manual_attempts = lesson_storage::load_manual_attempts(username, &course_id).map_err(ApiError::Internal)?;

    // 检查当前是否有正在处理的课节
    let processing_tasks: Vec<_> = state.pkuvideo_jobs.read().await.values()
        .filter(|j|j.user_id == current_user.id && j.course_id.as_deref() == Some(&course_id)
            && matches!(j.status, PipelineStatus::Pending | PipelineStatus::Running))
        .map(|j|serde_json::json!({"job_id":j.id,"lesson_id":j.lesson_id})).collect();
    let processing_video_ids: Vec<String> = processing_tasks.iter()
        .filter_map(|j|j["lesson_id"].as_str().map(str::to_string)).collect();
    let processing_video_id = processing_video_ids.first().cloned();

    tracing::info!("用户 {} 课程 {} 加载了 {} 个课节, processing: {:?}", username, course_id, lessons.len(), processing_video_id);

    Ok(Json(serde_json::json!({
        "success": true,
        "lessons": lessons,
        "manual_attempts": manual_attempts,
        "total": total,
        "processed": processed,
        "all_processed": total > 0 && processed == total,
        "processing_video_id": processing_video_id,
        "processing_video_ids": processing_video_ids,
        "processing_tasks": processing_tasks
    })))
}

async fn refresh_lessons(
    State(state): State<AppState>,
    axum::extract::Path((course_id,)): axum::extract::Path<(String,)>,
    current_user: CurrentUser,
) -> Result<Json<serde_json::Value>, ApiError> {
    let lesson_mutex = state.lesson_mutation(current_user.id).await;
    let _guard = lesson_mutex.lock().await;
    if state.course_busy(current_user.id, &course_id).await {
        return Err(ApiError::BadRequest("请等待当前课节处理完成后刷新".into()));
    }
    let user = state.store.get_user(current_user.id).await.ok_or_else(|| {
        tracing::error!("用户不存在: {}", current_user.id);
        ApiError::Internal("用户不存在".to_string())
    })?;

    let username = user.email.split('@').next().ok_or_else(|| {
        tracing::error!("邮箱格式无效: {}", user.email);
        ApiError::BadRequest("无效的邮箱格式".to_string())
    })?;

    let password = match user.iaaa_encrypted_password.clone() {
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

    let mut login_result = pkulogin::login_iaaa_and_get_cookies_with_cache(&username, &password, &state.login_cache).await
        .map_err(|e| {
            tracing::error!("IAAA登录失败: {}", e);
            ApiError::BadRequest(format!("IAAA登录失败: {}", e))
        })?;

    // One lease for all pages; retry the full read once only on authentication loss.
    let mut all_lessons = pkulogin::session_operation(
        &mut login_result, username, &password, &state.login_cache, |login| {
            let mut all_lessons = Vec::new();
            let mut current_url = format!("https://course.pku.edu.cn/webapps/bb-streammedia-hqy-BBLEARN/videoList.action?course_id={}&mode=view", course_id);
            let mut visited = HashSet::new();
            loop {
                if visited.len() >= MAX_VIDEO_LIST_PAGES || !visited.insert(current_url.clone()) {
                    return Err("课节分页异常：页数超限或重复跳转，已终止刷新".into());
                }
                let page = pkulogin::fetch_page_in_session(login, &current_url)?;
                // Require a list container, including the normal empty-list state.
                if !page.contains("listContainer") {
                    return Err("教学网返回的内容不是可识别的课节列表，已保留原有课节".into());
                }
                let mut lessons = lesson_parser::parse_lessons(&page, &course_id);
                if lessons.is_empty() && page.contains("listContainer_row:") {
                    return Err("课节列表解析失败，已保留原有课节".into());
                }
                all_lessons.append(&mut lessons);
                match lesson_parser::get_next_page_url(&page) {
                    Some(next) => current_url = next,
                    None => break,
                }
            }
            Ok(all_lessons)
        },
    ).await.map_err(ApiError::Internal)?;

    // 合并已有的课节数据：保留 video_url, is_processed, tex_filename，只更新 play_url
    lesson_parser::assign_lesson_numbers(&mut all_lessons);
    merge_lesson_data(&mut all_lessons, &existing_lessons);

    lesson_storage::init_course_dir(username, &course_id)
        .map_err(|e| ApiError::Internal(format!("创建课节目录失败: {}", e)))?;
    lesson_storage::save_ordered_lessons(username, &course_id, &existing_lessons, &mut all_lessons)
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
    let mut matched_ids = HashSet::new();
    for new_lesson in new_lessons.iter_mut() {
        // 查找日期和时间相同的已有课节
        if let Some(existing) = existing_lessons.iter().find(|l| l.manual_replay.is_none() && !matched_ids.contains(&l.id) && l.date == new_lesson.date && l.time == new_lesson.time) {
            matched_ids.insert(existing.id.clone());
            tracing::info!("找到匹配的课节: {} {}，保留原有数据", new_lesson.date, new_lesson.time);
            // 保留原有数据，只更新播放链接
            new_lesson.video_url = existing.video_url.clone();
            new_lesson.enc_key = existing.enc_key.clone();
            new_lesson.is_processed = existing.is_processed;
            new_lesson.tex_filename = existing.tex_filename.clone();
            new_lesson.id = existing.id.clone();
            // play_url 从新解析结果中获取（可能已更新）
        } else if !existing_lessons.is_empty() {
            new_lesson.id = format!("lesson_{}", Uuid::new_v4());
        }
    }
    new_lessons.extend(existing_lessons.iter().filter(|l| l.manual_replay.is_some()).cloned());
    lesson_parser::sort_lessons(new_lessons);
}

#[cfg(test)]
mod manual_refresh_tests {
    use super::*;

    #[test]
    fn refresh_retains_manual_origin_and_processed_ids() {
        let regular = LessonInfo {enc_key:None,id:"lesson_9".into(), video_url:"saved".into(), play_url:"old".into(),
            date:"2026-10-12".into(), time:"第1-2节".into(), day_of_week:"周一".into(),
            is_processed:true, tex_filename:Some("saved.tex".into()), manual_replay:None};
        let manual = LessonInfo {id:"app_stable".into(), manual_replay:Some(crate::services::app_replay::ManualReplay {
            course_id:"app-course".into(), sub_id:"app-sub".into(),date:"2026-10-12".into(),lesson_start:Some(1),lesson_end:Some(2)}), ..regular.clone()};
        let fresh = LessonInfo {id:"lesson_1".into(), video_url:String::new(), play_url:"new".into(),
            is_processed:false, tex_filename:None, ..regular.clone()};
        let mut new = vec![fresh];
        merge_lesson_data(&mut new, &vec![regular, manual]);
        assert_eq!(new.len(), 2);
        let regular = new.iter().find(|l| l.manual_replay.is_none()).unwrap();
        assert_eq!(regular.id, "lesson_9");
        assert_eq!(regular.play_url, "new");
        assert_eq!(regular.video_url, "saved");
        assert!(regular.is_processed);
        assert_eq!(regular.tex_filename.as_deref(), Some("saved.tex"));
        let manual = new.iter().find(|l| l.manual_replay.is_some()).unwrap();
        assert_eq!(manual.id, "app_stable");
        let mut empty = Vec::new();
        merge_lesson_data(&mut empty, &new);
        assert_eq!(empty.len(), 1);
        assert_eq!(empty[0].id, "app_stable");
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

    let password = match user.iaaa_encrypted_password.clone() {
        Some(p) => p,
        None => {
            tracing::warn!("用户未绑定IAAA账号: {}", current_user.id);
            return Err(ApiError::BadRequest("未绑定IAAA账号，请先绑定".to_string()));
        }
    };

    tracing::info!("用户 {} 开始刷新课程列表...", username);

    let mut login_result = pkulogin::login_iaaa_and_get_cookies_with_cache(&username, &password, &state.login_cache).await
        .map_err(|e| {
            tracing::error!("IAAA登录失败: {}", e);
            ApiError::BadRequest(format!("IAAA登录失败: {}", e))
        })?;

    let portal_url = "https://course.pku.edu.cn/webapps/portal/execute/tabs/tabAction?tab_tab_group_id=_1_1";
    let courses = pkulogin::session_operation(
        &mut login_result, username, &password, &state.login_cache, |login| {
            let page = pkulogin::fetch_page_in_session(login, portal_url)?;
            if !pkulogin::is_teaching_portal(&page) {
                return Err("教学网返回的内容不是可识别的课程页面，已保留原有课程".into());
            }
            let courses = course_parser::parse_courses(&page);
            if courses.is_empty() && page.contains("type=Course") {
                return Err("课程列表解析失败，已保留原有课程".into());
            }
            Ok(courses)
        },
    ).await.map_err(ApiError::Internal)?;

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
    State(state): State<AppState>, current_user: CurrentUser,
    Json(req): Json<BatchSubmitRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    use crate::services::batch_submission::{CourseSubmitter, BatchSubmitter};
    let gate = state.batch_submissions.gate(current_user.id).await;
    let _exclusive = gate.write().await;
    if state.batch_submissions.active(current_user.id).await {
        return Err(ApiError::Conflict("任务正在提交中，请等待当前批量提交完成".into()));
    }
    let user = state.store.get_user(current_user.id).await.ok_or(ApiError::Unauthorized)?;
    let username = user.email.split('@').next().unwrap_or_default();
    let lessons = lesson_storage::load_lessons(username, &req.course_id).map_err(ApiError::Internal)?;
    let requested: HashSet<_> = req.video_ids.into_iter().collect();
    if requested.iter().any(|id|!lessons.iter().any(|lesson|&lesson.id==id)) {
        return Err(ApiError::BadRequest("批量提交包含不存在的课节".into()));
    }
    let mut ids = Vec::new();
    for lesson in lessons {
        if (requested.is_empty() || requested.contains(&lesson.id)) && !lesson.is_processed
            && !state.lesson_busy(current_user.id,&req.course_id,&lesson.id).await { ids.push(lesson.id); }
    }
    if ids.is_empty() { return Err(ApiError::BadRequest("没有需要提交的课节，已处理或已入队的课节会自动跳过".into())); }
    let submitter: std::sync::Arc<dyn BatchSubmitter> = std::sync::Arc::new(CourseSubmitter { state:state.clone(),user_id:current_user.id });
    let batch = state.batch_submissions.start(current_user.id,req.course_id,ids,submitter).await.map_err(ApiError::Conflict)?;
    Ok(Json(serde_json::json!({"success":true,"batch":batch})))
}

async fn batch_submit_status(State(state): State<AppState>, current_user: CurrentUser)
    -> impl IntoResponse {
    let value = state.batch_submissions.snapshot(current_user.id).await
        .map(|batch|serde_json::to_value(batch).expect("batch snapshot"))
        .unwrap_or_else(||serde_json::json!({"active":false}));
    ([(axum::http::header::CACHE_CONTROL,"no-store")],Json(value))
}

async fn process_lesson(
    State(state): State<AppState>,
    axum::extract::Path((course_id, video_id)): axum::extract::Path<(String, String)>,
    current_user: CurrentUser,
    Json(req): Json<ProcessLessonRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    crate::services::lesson_submission::submit_lesson(state, current_user.id, course_id, video_id, req.manual_key).await
}

async fn get_process_status(
    State(state): State<AppState>,
    axum::extract::Path((course_id, video_id)): axum::extract::Path<(String, String)>,
    current_user: CurrentUser,
) -> Result<Json<serde_json::Value>, ApiError> {
    let jobs = state.pkuvideo_jobs.read().await;
    if let Some(job) = jobs.values().filter(|j|j.user_id == current_user.id
        && j.course_id.as_deref() == Some(&course_id) && j.lesson_id.as_deref() == Some(&video_id))
        .max_by_key(|j|j.created_at) {
        let (status, message) = match job.status {
            PipelineStatus::Pending => ("queued", "队列等待中".to_string()),
            PipelineStatus::Running => ("processing", if job.postprocessing {"正在保存课节笔记"} else {"正在处理视频"}.to_string()),
            PipelineStatus::Succeeded => ("completed", "处理完成".to_string()),
            PipelineStatus::Failed => ("failed", job.error.clone().unwrap_or_default()),
        };
        return Ok(Json(serde_json::json!({"status":status,"message":message,"job_id":job.id,"priority":job.priority,"steps":job.steps,"postprocessing":job.postprocessing,"result":job.course_notes_path})));
    }
    drop(jobs);
    let user = state.store.get_user(current_user.id).await.ok_or(ApiError::Unauthorized)?;
    let lessons = lesson_storage::load_lessons(user.email.split('@').next().unwrap_or_default(), &course_id).map_err(ApiError::Internal)?;
    let lesson = lessons.iter().find(|l|l.id == video_id).ok_or_else(||ApiError::BadRequest("课节不存在".into()))?;
    Ok(Json(serde_json::json!({"status":if lesson.is_processed {"completed"} else {"idle"}})))
}

async fn download_lesson_pdf(
    State(state): State<AppState>,
    axum::extract::Path((course_id, video_id)): axum::extract::Path<(String, String)>,
    current_user: CurrentUser,
) -> Result<impl IntoResponse, ApiError> {
    let lesson_mutex = state.lesson_mutation(current_user.id).await;
    let _guard = lesson_mutex.lock().await;
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
    let lesson_mutex = state.lesson_mutation(current_user.id).await;
    let _guard = lesson_mutex.lock().await;
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

    // 使用课节列表本身的顺序，避免字符串排序把第10节排在第2节之前。
    let mut lessons = lessons;
    lesson_parser::sort_lessons(&mut lessons);
    // 筛选已处理的课节（有tex文件）
    let processed_lessons: Vec<&LessonInfo> = lessons.iter()
        .filter(|l| l.tex_filename.is_some())
        .collect();

    if processed_lessons.is_empty() {
        return Err(ApiError::BadRequest("没有已处理的课节".to_string()));
    }

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

    // 复制封面图片到临时目录
    let cover_image_path = std::path::Path::new("./image/cover_1.jpg");
    if cover_image_path.exists() {
        tokio::fs::copy(cover_image_path, temp_dir_path.join("cover_1.jpg")).await.map_err(|e| {
            tracing::error!("复制封面图片失败: {}", e);
            ApiError::Internal(format!("复制封面图片失败: {}", e))
        })?;
        tracing::info!("已复制封面图片");
    } else {
        tracing::warn!("封面图片不存在: {:?}", cover_image_path);
    }

    // 构建完整的LaTeX文档内容
    let mut full_content = String::new();

    // 免责声明
    let disclaimer1 = "「严小希」小助手是由cxxdgc开发的人工智能助手，其生成内容不代表作者任何观点或立场";
    let disclaimer2 = "本笔记由人工智能根据课程回放生成，可能存在疏漏或偏差。如有冲突或疑问，请务必以课程回放、教材与PPT为准进行学习";

    // 封面和目录部分 - 使用elegantbook模板
    full_content.push_str(r#"\documentclass[lang=cn,10pt,scheme=chinese,a4paper]{elegantbook}

\cover{cover_1.jpg}

\title{"#);
    full_content.push_str(&format!("{}笔记", course_name));
    full_content.push_str(r#"}
\author{严小希}
\extrainfo{"#);
    full_content.push_str(&format!("{}\\\\{}", disclaimer1, disclaimer2));
    full_content.push_str(r#"}

\begin{document}

\maketitle

\tableofcontents

\pagenumbering{arabic}
\setcounter{page}{1}

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

/// 下载课节的tex文件
async fn download_lesson_tex(
    State(state): State<AppState>,
    axum::extract::Path((course_id, video_id)): axum::extract::Path<(String, String)>,
    current_user: CurrentUser,
) -> Result<impl IntoResponse, ApiError> {
    let lesson_mutex = state.lesson_mutation(current_user.id).await;
    let _guard = lesson_mutex.lock().await;
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

    // 读取tex内容
    let tex_data = tokio::fs::read(&tex_path)
        .await
        .map_err(|e| ApiError::Internal(format!("读取tex文件失败: {}", e)))?;

    // 构建响应
    let response = axum::response::Response::builder()
        .header("Content-Type", "application/x-tex")
        .header("Content-Disposition", format!("attachment; filename=\"{}\"", tex_filename))
        .header("Content-Length", tex_data.len())
        .body(axum::body::Body::from(tex_data))
        .map_err(|e| ApiError::Internal(format!("构建响应失败: {}", e)))?;

    Ok(response)
}

/// 重新处理课节（删除旧的tex文件并重新处理）
async fn reprocess_lesson(
    State(state): State<AppState>,
    axum::extract::Path((course_id, video_id)): axum::extract::Path<(String, String)>,
    current_user: CurrentUser,
) -> Result<Json<serde_json::Value>, ApiError> {
    let lesson_mutex = state.lesson_mutation(current_user.id).await;
    let _guard = lesson_mutex.lock().await;
    if state.course_busy(current_user.id, &course_id).await {
        return Err(ApiError::BadRequest("请等待当前课节处理完成".into()));
    }
    let user = state.store.get_user(current_user.id).await.ok_or_else(|| {
        tracing::error!("用户不存在: {}", current_user.id);
        ApiError::Internal("用户不存在".to_string())
    })?;

    let username = user.email.split('@').next().ok_or_else(|| {
        tracing::error!("邮箱格式无效: {}", user.email);
        ApiError::BadRequest("无效的邮箱格式".to_string())
    })?;

    // 加载课节信息
    let mut lessons = lesson_storage::load_lessons(username, &course_id)
        .map_err(|e| {
            tracing::error!("加载课节信息失败: {}", e);
            ApiError::Internal(format!("加载课节信息失败: {}", e))
        })?;

    // 找到对应的课节
    let lesson_index = lessons.iter().position(|l| l.id == video_id)
        .ok_or_else(|| ApiError::BadRequest("课节不存在".to_string()))?;

    let old_lessons = lessons.clone();

    // 重置课节的处理状态
    lessons[lesson_index].tex_filename = None;
    lessons[lesson_index].is_processed = false;

    // 保存更新后的课节信息
    lesson_storage::save_ordered_lessons(username, &course_id, &old_lessons, &mut lessons)
        .map_err(|e| {
            tracing::error!("保存课节信息失败: {}", e);
            ApiError::Internal(format!("保存课节信息失败: {}", e))
        })?;

    tracing::info!("用户 {} 已重置课节 {} 的处理状态，准备重新处理", username, video_id);

    Ok(Json(serde_json::json!({
        "success": true,
        "message": "已重置处理状态，请点击\"提交处理\"重新处理该课节"
    })))
}

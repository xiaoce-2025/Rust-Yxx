use crate::{
    app_state::AppState,
    auth::CurrentUser,
    error::ApiError,
    services::{
        app_replay::{self, ManualReplay, SemesterConfig},
        course_storage,
        lesson_storage::{self, LessonInfo, ManualAttempt},
    },
};
use axum::{
    extract::{Path, State},
    routing::{delete, get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/pkuvideofullclass/app-auth/:action", post(app_auth))
        .route(
            "/api/pkuvideofullclass/courses/:course_id/manual-options",
            get(options),
        )
        .route(
            "/api/pkuvideofullclass/courses/:course_id/manual-lessons",
            post(add),
        )
        .route(
            "/api/pkuvideofullclass/courses/:course_id/manual-lessons/:lesson_id",
            delete(delete_lesson),
        )
}

async fn app_auth(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(action): Path<String>,
    Json(mut request): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    if !["prepare", "captcha", "sms", "login", "status", "reset"].contains(&action.as_str()) {
        return Err(ApiError::BadRequest("未知认证操作".into()));
    }
    if action == "reset" {
        app_replay::clear_session(current.id).map_err(ApiError::Internal)?;
        state.app_replay_clients.lock().await.remove(&current.id);
        return Ok(Json(json!({"ok":true,"data":{"logged_in":false}})));
    }
    let user = state
        .store
        .get_user(current.id)
        .await
        .ok_or(ApiError::Unauthorized)?;
    let object = request
        .as_object_mut()
        .ok_or_else(|| ApiError::BadRequest("请求应为对象".into()))?;
    object.insert(
        "account".into(),
        json!(user.email.split('@').next().unwrap_or_default()),
    );
    object.insert("action".into(), json!(action));
    let client = state
        .app_replay_client(current.id)
        .await
        .map_err(ApiError::Internal)?;
    let mut client = client.lock().await;
    let response = if action == "login" || action == "status" {
        client.login_bound(&user, &request).await
    } else {
        client.request(request).await
    }
    .map_err(ApiError::Internal)?;
    Ok(Json(response))
}

async fn options(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(course_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let user = state
        .store
        .get_user(current.id)
        .await
        .ok_or(ApiError::Unauthorized)?;
    let username = user.email.split('@').next().unwrap_or_default();
    let course = course_storage::load_courses(username)
        .map_err(ApiError::Internal)?
        .into_iter()
        .find(|c| c.key == course_id)
        .ok_or(ApiError::NotFound)?;
    let config = SemesterConfig::load().map_err(ApiError::BadRequest)?;
    let range = config
        .range(&course.term, app_replay::today())
        .map_err(ApiError::BadRequest)?;
    Ok(Json(
        json!({"success":true,"term":course.term,"start":range.start,"end":range.end}),
    ))
}

#[derive(Deserialize)]
struct AddRequest {
    lesson_start: u8,
    lesson_end: u8,
    #[serde(default)]
    weekdays: Vec<u32>,
    date: Option<chrono::NaiveDate>,
    #[serde(default)]
    retry_failed: bool,
}

async fn add(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(course_id): Path<String>,
    Json(request): Json<AddRequest>,
) -> Result<Json<Value>, ApiError> {
    app_replay::validate_periods(request.lesson_start, request.lesson_end)
        .map_err(ApiError::BadRequest)?;
    let user = state
        .store
        .get_user(current.id)
        .await
        .ok_or(ApiError::Unauthorized)?;
    let username = user.email.split('@').next().unwrap_or_default().to_string();
    let course = course_storage::load_courses(&username)
        .map_err(ApiError::Internal)?
        .into_iter()
        .find(|c| c.key == course_id)
        .ok_or(ApiError::NotFound)?;
    let config = SemesterConfig::load().map_err(ApiError::BadRequest)?;
    let range = config
        .range(&course.term, app_replay::today())
        .map_err(ApiError::BadRequest)?;
    let dates = app_replay::selected_dates(&range, &request.weekdays, request.date)
        .map_err(ApiError::BadRequest)?;
    let lesson_mutex = state.lesson_mutation(current.id).await;
    let _guard = lesson_mutex.lock().await;
    if state.course_busy(current.id, &course_id).await
    {
        return Err(ApiError::BadRequest("请等待当前课节处理完成".into()));
    }
    let mut attempts =
        lesson_storage::load_manual_attempts(&username, &course_id).map_err(ApiError::Internal)?;
    let pending: Vec<String> = dates
        .iter()
        .filter(|date| lesson_storage::should_query_date(&attempts, date, request.retry_failed))
        .cloned()
        .collect();
    let skipped = dates.len() - pending.len();
    if pending.is_empty() {
        return Ok(Json(
            json!({"success":true,"added":0,"failed":0,"skipped":skipped,
            "message":"所选日期已有手动查询记录，已跳过"}),
        ));
    }
    let client = state
        .app_replay_client(current.id)
        .await
        .map_err(ApiError::Internal)?;
    let mut client = client.lock().await;
    let status = client
        .login_bound(&user, &json!({}))
        .await
        .map_err(ApiError::Internal)?;
    if status["data"]["logged_in"] != true {
        return Ok(Json(
            json!({"success":false,"needs_app_login":true,"message":status["error"].as_str().unwrap_or("好学认证需要验证码")}),
        ));
    }
    for date in &pending {
        let record = ManualAttempt {
            date: date.clone(),
            status: "pending".into(),
            message: "已提交查询；如服务中断，可明确重试".into(),
            updated_at: chrono::Utc::now().to_rfc3339(),
        };
        if let Some(a) = attempts.iter_mut().find(|a| a.date == *date) {
            *a = record;
        } else {
            attempts.push(record);
        }
    }
    // Save before the API request: crashes and failures must not cause silent repetition.
    lesson_storage::save_manual_attempts(&username, &course_id, &attempts)
        .map_err(ApiError::Internal)?;
    let scan = client
        .request(
            json!({"action":"scan", "dates":pending,"course_name":course.name,
        "app_course_id":config.app_course_ids.get(&course_id).cloned().unwrap_or_default()}),
        )
        .await;
    let response = match scan {
        Ok(r) if r["ok"] == true => r["data"].clone(),
        other => {
            let message = match other {
                Ok(r) => r["error"].as_str().unwrap_or("App 查询失败").to_string(),
                Err(e) => e,
            };
            for a in attempts.iter_mut().filter(|a| pending.contains(&a.date)) {
                a.status = "failed".into();
                a.message = message.clone();
            }
            lesson_storage::save_manual_attempts(&username, &course_id, &attempts)
                .map_err(ApiError::Internal)?;
            return Err(ApiError::BadRequest(message));
        }
    };
    let mut lessons =
        lesson_storage::load_lessons(&username, &course_id).map_err(ApiError::Internal)?;
    let old_lessons = lessons.clone();
    let mut added = 0;
    let mut failed = 0;
    for result in response
        .as_array()
        .ok_or_else(|| ApiError::Internal("App 查询返回格式异常".into()))?
    {
        let date = result["date"].as_str().unwrap_or_default();
        let attempt = attempts
            .iter_mut()
            .find(|a| a.date == date)
            .ok_or_else(|| ApiError::Internal("返回日期不在查询范围".into()))?;
        let mut errors = Vec::new();
        if let Some(error) = result["error"].as_str() {
            errors.push(error.to_string());
        }
        if let Some(rows) = result["lessons"].as_array() {
            for row in rows {
                if let Some(error) = row["error"].as_str() {
                    errors.push(error.to_string());
                    continue;
                }
                let replay = ManualReplay {
                    course_id: row["course_id"].as_str().unwrap_or_default().into(),
                    sub_id: row["sub_id"].as_str().unwrap_or_default().into(),
                    date: date.into(),
                    lesson_start: Some(request.lesson_start),
                    lesson_end: Some(request.lesson_end),
                };
                if lessons.iter().any(|l| {
                    l.manual_replay.as_ref().is_some_and(|r| {
                        r.course_id == replay.course_id && r.sub_id == replay.sub_id
                    })
                }) {
                    continue;
                }
                let parsed: chrono::NaiveDate = date
                    .parse()
                    .map_err(|_| ApiError::Internal("返回日期无效".into()))?;
                use chrono::Datelike;
                lessons.push(LessonInfo {
                enc_key: None,
                id: format!("app_{}", Uuid::new_v4()),
                    video_url: row["video_url"].as_str().unwrap_or_default().into(),
                    play_url: String::new(),
                    date: date.into(),
                    time: format!("第{}-{}节", request.lesson_start, request.lesson_end),
                    day_of_week: format!(
                        "周{}",
                        ["一", "二", "三", "四", "五", "六", "日"]
                            [parsed.weekday().num_days_from_monday() as usize]
                    ),
                    is_processed: false,
                    tex_filename: None,
                    manual_replay: Some(replay),
                });
                added += 1;
            }
        }
        attempt.status = if errors.is_empty() {
            "success"
        } else {
            failed += 1;
            "failed"
        }
        .into();
        attempt.message = if errors.is_empty() {
            "已查询并添加该日期回放".into()
        } else {
            errors.join("；")
        };
        attempt.updated_at = chrono::Utc::now().to_rfc3339();
    }
    lesson_storage::save_ordered_lessons(&username, &course_id, &old_lessons, &mut lessons)
        .map_err(ApiError::Internal)?;
    lesson_storage::save_manual_attempts(&username, &course_id, &attempts)
        .map_err(ApiError::Internal)?;
    Ok(Json(
        json!({"success":true,"added":added,"failed":failed,"skipped":skipped,
        "message":format!("新增 {added} 个课节，{failed} 个日期失败，跳过 {skipped} 个已查询日期")}),
    ))
}

#[derive(Deserialize)]
struct DeleteRequest {
    confirmation: String,
}

async fn delete_lesson(
    State(state): State<AppState>,
    current: CurrentUser,
    Path((course_id, lesson_id)): Path<(String, String)>,
    Json(request): Json<DeleteRequest>,
) -> Result<Json<Value>, ApiError> {
    if request.confirmation != "确认删除" {
        return Err(ApiError::BadRequest("请输入“确认删除”完成二次确认".into()));
    }
    let lesson_mutex = state.lesson_mutation(current.id).await;
    let _guard = lesson_mutex.lock().await;
    if state.course_busy(current.id, &course_id).await
    {
        return Err(ApiError::BadRequest("请等待当前课节处理完成".into()));
    }
    let user = state
        .store
        .get_user(current.id)
        .await
        .ok_or(ApiError::Unauthorized)?;
    let username = user.email.split('@').next().unwrap_or_default();
    if !course_storage::load_courses(username)
        .map_err(ApiError::Internal)?
        .iter()
        .any(|c| c.key == course_id)
    {
        return Err(ApiError::NotFound);
    }
    lesson_storage::delete_manual_lesson(username, &course_id, &lesson_id, &request.confirmation)
        .map_err(ApiError::BadRequest)?;
    Ok(Json(
        json!({"success":true,"message":"手动课节已删除，笔记文件已按实际顺序重新编号"}),
    ))
}

//! Authenticated preparation and queue submission for one lesson.
use super::{lesson_storage, pkulogin, pkuvideo::PkuVideoJob, video_downloader};
use crate::{app_state::AppState, error::ApiError};
use axum::Json;
use uuid::Uuid;

pub async fn submit_lesson(
    state: AppState,
    user_id: Uuid,
    course_id: String,
    video_id: String,
    manual_key: Option<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let lesson_mutex = state.lesson_mutation(user_id).await;
    let _mutation_guard = lesson_mutex.lock().await;
    if state.lesson_busy(user_id, &course_id, &video_id).await {
        return Err(ApiError::BadRequest("该课节已在队列中或正在处理".into()));
    }
    let user = state
        .store
        .get_user(user_id)
        .await
        .ok_or(ApiError::Unauthorized)?;
    let username = user.email.split('@').next().unwrap_or_default().to_string();
    let mut lessons =
        lesson_storage::load_lessons(&username, &course_id).map_err(ApiError::Internal)?;
    let index = lessons
        .iter()
        .position(|l| l.id == video_id)
        .ok_or_else(|| ApiError::BadRequest("课节不存在".into()))?;
    let supplied_key = manual_key.or_else(|| lessons[index].enc_key.clone());
    let media = if !lessons[index].video_url.is_empty() && supplied_key.is_some() {
        crate::services::prepared_media::prepare_teaching(
            &lessons[index].video_url,
            "",
            "",
            &state.login_cache,
            supplied_key.as_deref(),
        )
        .await
        .map_err(ApiError::BadRequest)?
    } else if let Some(mut replay) = lessons[index].manual_replay.clone() {
        if replay.date.is_empty() {
            replay.date = lessons[index].date.clone();
        }
        let client = state
            .app_replay_client(user_id)
            .await
            .map_err(ApiError::Internal)?;
        let mut client = client.lock().await;
        let status = client
            .login_bound(&user, &serde_json::json!({}))
            .await
            .map_err(ApiError::Internal)?;
        if status["data"]["logged_in"] != true {
            return Ok(Json(
                serde_json::json!({"success":false,"needs_app_login":true,
                "message":status["error"].as_str().unwrap_or("好学认证需要验证码")}),
            ));
        }
        client
            .prepare_media(&replay)
            .await
            .map_err(ApiError::BadRequest)?
    } else {
        let password = user
            .iaaa_encrypted_password
            .as_deref()
            .ok_or_else(|| ApiError::BadRequest("未绑定IAAA账号，请先绑定".into()))?;
        let play_url = lessons[index].play_url.clone();
        if play_url.is_empty() {
            return Err(ApiError::BadRequest(
                "播放链接为空，请先刷新课节列表".into(),
            ));
        }
        let mut login = pkulogin::login_iaaa_and_get_cookies_with_cache(
            &username,
            password,
            &state.login_cache,
        )
        .await
        .map_err(ApiError::BadRequest)?;
        let (url, key_uri, key) = pkulogin::session_operation(
            &mut login,
            &username,
            password,
            &state.login_cache,
            |session| {
                // Retry the entire watching-page -> signed URL -> key transaction after authentication expires.
                let url = video_downloader::get_video_download_url(&play_url, session)?;
                if !url.contains(".m3u8") {
                    return Ok((url, String::new(), Ok(None)));
                }
                if let Some(key) = &supplied_key {
                    crate::services::prepared_media::decode_key(key)?;
                    return Ok((url, String::new(), Ok(Some(key.clone()))));
                }
                let (uri, key) =
                    crate::services::pkuvideo::fetch_m3u8_key_in_session(&url, session)?;
                if let Err(error) = &key {
                    if pkulogin::is_auth_error(error) {
                        return Err(error.clone());
                    }
                }
                Ok((url, uri, key.map(Some)))
            },
        )
        .await
        .map_err(ApiError::BadRequest)?;
        lessons[index].video_url = url.clone();
        match key {
            Ok(enc_key) => crate::services::prepared_media::PreparedMedia { url, enc_key },
            Err(_) => {
                lesson_storage::save_lessons(&username, &course_id, &lessons)
                    .map_err(ApiError::Internal)?;
                return Ok(Json(
                    serde_json::json!({"success":true,"needs_key":true,"key_uri":key_uri,"video_url":url}),
                ));
            }
        }
    };
    lessons[index].video_url = media.url.clone();
    lessons[index].enc_key = media.enc_key.clone();
    lesson_storage::save_lessons(&username, &course_id, &lessons).map_err(ApiError::Internal)?;
    let job_id = Uuid::new_v4();
    let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
    let mut job = PkuVideoJob::new(
        job_id,
        user_id,
        media.url.clone(),
        format!("./data/pkuvideo_jobs/{timestamp}_{job_id}"),
    );
    job.course_id = Some(course_id.clone());
    job.lesson_id = Some(video_id.clone());
    state
        .store
        .insert_pkuvideo_job(job.clone())
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    state.pkuvideo_jobs.write().await.insert(job_id, job);
    if let Err(error) =
        crate::services::video_jobs::enqueue_prepared_job(&state, job_id, user_id, media).await
    {
        state.pkuvideo_jobs.write().await.remove(&job_id);
        let _ = state.store.delete_pkuvideo_job(job_id).await;
        return Err(ApiError::Internal(error));
    }
    Ok(Json(
        serde_json::json!({"success":true,"job_id":job_id,"task_id":job_id,
        "status_url":format!("/api/pkuvideo/jobs/{job_id}"),
        "status":"queued","message":"已加入视频处理队列"}),
    ))
}

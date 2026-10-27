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
        let lesson = lessons[index].clone();
        let prepared = fallback_once(
            prepare_teaching_media(&state, &user, &username, &lesson, supplied_key.as_deref()),
            || prepare_app_course_media(&state, &user, &username, &course_id, &lesson),
        )
        .await
        .map_err(ApiError::BadRequest)?;
        match prepared {
            Preparation::Media(media) => media,
            Preparation::NeedsKey { url, key_uri } => {
                lessons[index].video_url = url.clone();
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

// Neither path changes lesson identity or manual_replay. The fallback is a transport choice only.
enum Preparation {
    Media(super::prepared_media::PreparedMedia),
    NeedsKey { url: String, key_uri: String },
}
async fn fallback_once<W, A, AF>(web: W, app: A) -> Result<Preparation, String>
where
    W: std::future::Future<Output = Result<Preparation, String>>,
    A: FnOnce() -> AF,
    AF: std::future::Future<Output = Result<super::prepared_media::PreparedMedia, String>>,
{
    match web.await {
        Ok(prepared) => Ok(prepared),
        Err(web_error) => {
            tracing::warn!(error=%web_error,"教学网获取回放链接失败，检查 App 登录状态并回退尝试一次");
            match app().await {
                Ok(media) => {
                    tracing::info!("好学 App 回退获取成功，保留原教学网课节来源");
                    Ok(Preparation::Media(media))
                }
                Err(app_error) => Err(format!(
                    "教学网获取失败：{web_error}；好学 App 回退也失败：{app_error}"
                )),
            }
        }
    }
}
async fn prepare_teaching_media(
    state: &AppState,
    user: &crate::store::FileUser,
    username: &str,
    lesson: &lesson_storage::LessonInfo,
    supplied_key: Option<&str>,
) -> Result<Preparation, String> {
    let password = user
        .iaaa_encrypted_password
        .as_deref()
        .ok_or("未绑定IAAA账号，请先绑定")?;
    if lesson.play_url.is_empty() {
        return Err("播放链接为空，请先刷新课节列表".into());
    }
    let mut login =
        pkulogin::login_iaaa_and_get_cookies_with_cache(username, password, &state.login_cache)
            .await?;
    let (url, key_uri, key) = pkulogin::session_operation(
        &mut login,
        username,
        password,
        &state.login_cache,
        |session| {
            let url = video_downloader::get_video_download_url(&lesson.play_url, session)?;
            if !url.contains(".m3u8") {
                return Ok((url, String::new(), Ok(None)));
            }
            if let Some(key) = supplied_key {
                super::prepared_media::decode_key(key)?;
                return Ok((url, String::new(), Ok(Some(key.to_string()))));
            }
            let (uri, key) = super::pkuvideo::fetch_m3u8_key_in_session(&url, session)?;
            if let Err(error) = &key {
                if pkulogin::is_auth_error(error) {
                    return Err(error.clone());
                }
            }
            Ok((url, uri, key.map(Some)))
        },
    )
    .await?;
    match key {
        Ok(enc_key) => Ok(Preparation::Media(super::prepared_media::PreparedMedia {
            url,
            enc_key,
        })),
        Err(_) => Ok(Preparation::NeedsKey { url, key_uri }),
    }
}
async fn prepare_app_course_media(
    state: &AppState,
    user: &crate::store::FileUser,
    username: &str,
    course_id: &str,
    lesson: &lesson_storage::LessonInfo,
) -> Result<super::prepared_media::PreparedMedia, String> {
    let course = super::course_storage::load_courses(username)?
        .into_iter()
        .find(|course| course.key == course_id)
        .ok_or("本地缺少课程信息，无法匹配好学 App 课程，请刷新课程列表")?;
    let mapping = match super::app_replay::SemesterConfig::load() {
        Ok(config) => config
            .app_course_ids
            .get(course_id)
            .cloned()
            .unwrap_or_default(),
        Err(error) => {
            tracing::warn!(%error,"未加载 App 课程映射，使用课程名称自动匹配");
            String::new()
        }
    };
    let client = state.app_replay_client(user.id).await?;
    let result = client
        .lock()
        .await
        .prepare_course_lesson(user, &course.name, &mapping, &lesson.date, &lesson.time)
        .await;
    result
}

#[cfg(test)]
mod tests {
    use super::super::prepared_media::PreparedMedia;
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[tokio::test]
    async fn fallback_is_attempted_once_and_does_not_mark_lesson_manual() {
        let calls = AtomicUsize::new(0);
        let prepared = fallback_once(async { Err("teaching link failure".into()) }, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(PreparedMedia {
                url: "https://media.example/video.m3u8".into(),
                enc_key: Some("0123456789abcdef".into()),
            })
        })
        .await
        .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let mut lesson:lesson_storage::LessonInfo=serde_json::from_value(serde_json::json!({
            "id":"regular-lesson","video_url":"","play_url":"original-watching-page","date":"2026-10-13",
            "time":"第1-2节","day_of_week":"周二","is_processed":false,"tex_filename":null
        })).unwrap();
        if let Preparation::Media(media) = prepared {
            lesson.video_url = media.url;
            lesson.enc_key = media.enc_key;
        } else {
            panic!("expected media")
        }
        assert!(lesson.manual_replay.is_none());
        assert_eq!(lesson.id, "regular-lesson");
        assert_eq!(lesson.play_url, "original-watching-page");
        assert!(lesson.enc_key.is_some());
        let failed = fallback_once(async { Err("teaching failure".into()) }, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err("app not logged in".into())
        })
        .await
        .err()
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(failed.contains("teaching failure") && failed.contains("app not logged in"));
    }
    #[tokio::test]
    async fn teaching_success_and_manual_key_prompt_do_not_trigger_app_fallback() {
        let result = fallback_once(
            async {
                Ok(Preparation::NeedsKey {
                    url: "url".into(),
                    key_uri: "key".into(),
                })
            },
            || async {
                panic!("a valid teaching link with a key prompt must not trigger the link fallback")
            },
        )
        .await;
        assert!(matches!(result, Ok(Preparation::NeedsKey { .. })));
        let result = fallback_once(
            async {
                Ok(Preparation::Media(PreparedMedia {
                    url: "video".into(),
                    enc_key: None,
                }))
            },
            || async { panic!("teaching success must not access the App") },
        )
        .await;
        assert!(matches!(result, Ok(Preparation::Media(_))));
    }
}

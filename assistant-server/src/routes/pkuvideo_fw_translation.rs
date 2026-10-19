use crate::{
    app_state::AppState,
    auth::CurrentUser,
    error::ApiError,
    services::{
        pkuvideo_fw::PipelineStatus,
        video_translation::{self as translation, TranslationJob},
    },
};
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::Response,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
use uuid::Uuid;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/pkuvideo_fw/jobs/:id/translation",
            get(detail).post(submit),
        )
        .route("/api/pkuvideo_fw/jobs/:id/translation/retry", post(retry))
        .route(
            "/api/pkuvideo_fw/jobs/:id/translation/video.mp4",
            get(video),
        )
        .route(
            "/api/pkuvideo_fw/jobs/:id/translation/subtitles/:kind",
            get(subtitle),
        )
}

async fn owner(state: &AppState, id: Uuid, user: Uuid) -> Result<(), ApiError> {
    let jobs = state.pkuvideo_fw_jobs.read().await;
    let job = jobs.get(&id).ok_or(ApiError::NotFound)?;
    if job.user_id != user {
        return Err(ApiError::Forbidden("无权访问此视频".into()));
    }
    if job.status != PipelineStatus::Succeeded || job.video_path.is_none() {
        return Err(ApiError::Conflict("请等待原视频下载完成".into()));
    }
    Ok(())
}

fn summary(job: &TranslationJob) -> Value {
    let completed = job.blocks.iter().filter(|b| b.done).count();
    json!({"status":job.status,"stage":job.stage,"error":job.error,"updated_at":job.updated_at,
        "completed_blocks":completed,"total_blocks":job.blocks.len(),"subtitle_precision":job.subtitle_precision,
        "video_url":if job.status == "succeeded" { Some(format!("/api/pkuvideo_fw/jobs/{}/translation/video.mp4",job.id)) } else { None }})
}

async fn detail(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    owner(&state, id, user.id).await?;
    let job = if let Some(entry) = state.video_translations.entry(id).await {
        Some(summary(&*entry.job.lock().await))
    } else {
        None
    };
    Ok(Json(
        json!({"enabled":state.settings.video_translation.enabled,"translation":job}),
    ))
}

async fn begin(
    state: AppState,
    user: CurrentUser,
    id: Uuid,
    retry: bool,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let slot = state.fw_slot(user.id).await;
    let _guard = slot.lock().await;
    owner(&state, id, user.id).await?;
    let job = translation::start(&state, id, retry)
        .await
        .map_err(ApiError::BadRequest)?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({"translation":summary(&job)})),
    ))
}

async fn submit(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<Uuid>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    begin(state, user, id, false).await
}
async fn retry(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<Uuid>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    begin(state, user, id, true).await
}

#[derive(Deserialize)]
struct Download {
    download: Option<String>,
}

async fn ready(state: &AppState, id: Uuid, user: Uuid) -> Result<(), ApiError> {
    owner(state, id, user).await?;
    let entry = state
        .video_translations
        .entry(id)
        .await
        .ok_or(ApiError::NotFound)?;
    let job = entry.job.lock().await;
    if job.user_id != user {
        return Err(ApiError::Forbidden("无权访问翻译结果".into()));
    }
    if job.status != "succeeded" {
        return Err(ApiError::Conflict("译制视频尚未完成或正在删除".into()));
    }
    Ok(())
}

async fn video(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<Uuid>,
    Query(query): Query<Download>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    ready(&state, id, user.id).await?;
    serve(
        translation::dir(id).join("translated.mp4"),
        "video/mp4",
        &format!("translated_{id}.mp4"),
        query.download.as_deref() == Some("1"),
        headers,
    )
    .await
}

async fn subtitle(
    State(state): State<AppState>,
    user: CurrentUser,
    Path((id, kind)): Path<(Uuid, String)>,
) -> Result<Response, ApiError> {
    ready(&state, id, user.id).await?;
    const KINDS: &[&str] = &[
        "en.source",
        "en.dub",
        "zh.source",
        "zh.dub",
        "bilingual.source",
        "bilingual.dub",
    ];
    let kind = kind.strip_suffix(".vtt").unwrap_or(&kind);
    if !KINDS.contains(&kind) {
        return Err(ApiError::NotFound);
    }
    serve(
        translation::dir(id).join(format!("{kind}.vtt")),
        "text/vtt; charset=utf-8",
        "subtitles.vtt",
        false,
        HeaderMap::new(),
    )
    .await
}

fn byte_range(value: &str, length: u64) -> Option<(u64, u64)> {
    let value = value.strip_prefix("bytes=")?;
    if length == 0 || value.contains(',') {
        return None;
    }
    let (start, end) = value.split_once('-')?;
    if start.is_empty() {
        let count: u64 = end.parse().ok()?;
        if count == 0 {
            return None;
        }
        return Some((length.saturating_sub(count), length - 1));
    }
    let start: u64 = start.parse().ok()?;
    let end = if end.is_empty() {
        length - 1
    } else {
        end.parse::<u64>().ok()?.min(length - 1)
    };
    if start >= length || start > end {
        None
    } else {
        Some((start, end))
    }
}

async fn serve(
    path: std::path::PathBuf,
    content_type: &str,
    filename: &str,
    download: bool,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|_| ApiError::NotFound)?;
    let length = file.metadata().await.map_err(|_| ApiError::NotFound)?.len();
    let range = headers.get(header::RANGE);
    let selected = range
        .and_then(|v| v.to_str().ok())
        .and_then(|v| byte_range(v, length));
    if range.is_some() && selected.is_none() {
        return Response::builder()
            .status(StatusCode::RANGE_NOT_SATISFIABLE)
            .header(header::CONTENT_RANGE, format!("bytes */{length}"))
            .body(Body::empty())
            .map_err(|e| ApiError::Internal(e.to_string()));
    }
    let (start, count) = selected.map(|(s, e)| (s, e - s + 1)).unwrap_or((0, length));
    file.seek(SeekFrom::Start(start))
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let mut response = Response::builder()
        .status(if selected.is_some() {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        })
        .header(header::CONTENT_TYPE, content_type)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, count)
        .header(header::CACHE_CONTROL, "private, no-store")
        .header(
            header::CONTENT_DISPOSITION,
            format!(
                "{}; filename=\"{filename}\"",
                if download { "attachment" } else { "inline" }
            ),
        );
    if let Some((s, e)) = selected {
        response = response.header(header::CONTENT_RANGE, format!("bytes {s}-{e}/{length}"));
    }
    response
        .body(Body::from_stream(tokio_util::io::ReaderStream::new(
            file.take(count),
        )))
        .map_err(|e| ApiError::Internal(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn streams_only_requested_bytes_and_returns_416() {
        let temp = tempdir::TempDir::new_in("target", "translation-range").unwrap();
        let path = temp.path().join("file.mp4");
        tokio::fs::write(&path, b"0123456789").await.unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, "bytes=2-5".parse().unwrap());
        let response = serve(path.clone(), "video/mp4", "video.mp4", true, headers)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 2-5/10");
        assert!(response.headers()[header::CONTENT_DISPOSITION]
            .to_str()
            .unwrap()
            .starts_with("attachment"));
        assert_eq!(
            &axum::body::to_bytes(response.into_body(), 100)
                .await
                .unwrap()[..],
            b"2345"
        );
        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, "bytes=20-".parse().unwrap());
        assert_eq!(
            serve(path, "video/mp4", "video.mp4", false, headers)
                .await
                .unwrap()
                .status(),
            StatusCode::RANGE_NOT_SATISFIABLE
        );
    }

    #[tokio::test]
    async fn rejects_other_users_and_unfinished_source_videos() {
        let (state, _temp) = translation::test_state().await;
        let id = Uuid::new_v4();
        let user = Uuid::new_v4();
        let mut job = crate::services::pkuvideo_fw::PkuVideoFwJob::new(id, user, String::new());
        state.pkuvideo_fw_jobs.write().await.insert(id, job.clone());
        assert!(matches!(
            owner(&state, id, Uuid::new_v4()).await,
            Err(ApiError::Forbidden(_))
        ));
        assert!(matches!(
            owner(&state, id, user).await,
            Err(ApiError::Conflict(_))
        ));
        job.status = PipelineStatus::Succeeded;
        job.video_path = Some("source.mp4".into());
        state.pkuvideo_fw_jobs.write().await.insert(id, job);
        owner(&state, id, user).await.unwrap();
        assert!(matches!(
            ready(&state, id, user).await,
            Err(ApiError::NotFound)
        ));
    }
    #[test]
    fn ranges_cover_suffix_open_and_invalid_requests() {
        assert_eq!(byte_range("bytes=-500", 100), Some((0, 99)));
        assert_eq!(byte_range("bytes=12-", 100), Some((12, 99)));
        assert_eq!(byte_range("bytes=12-500", 100), Some((12, 99)));
        for (v, n) in [
            ("bytes=0-", 0),
            ("bytes=-0", 100),
            ("bytes=100-", 100),
            ("bytes=9-2", 100),
            ("bytes=0-1,3-4", 100),
        ] {
            assert_eq!(byte_range(v, n), None);
        }
    }
}

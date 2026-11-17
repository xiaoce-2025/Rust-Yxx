//! Authenticated feature flags and metadata-only access audit.
use crate::{app_state::AppState, auth::CurrentUser, error::ApiError};
use axum::{
    extract::{FromRequestParts, Request, State},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/site/features", get(features))
}
async fn features(
    State(state): State<AppState>,
    _user: CurrentUser,
) -> Result<impl IntoResponse, ApiError> {
    Ok((
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Json(
            state
                .store
                .site_features()
                .await
                .map_err(ApiError::Internal)?,
        ),
    ))
}
pub async fn audit(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let is_note = method == axum::http::Method::GET
        && (path.ends_with("/download-pdf")
            || path.ends_with("/download-tex")
            || (path.starts_with("/api/pkuvideo/jobs/") && (path.ends_with("/pdf") || path.ends_with("/tex")))
            || (path.starts_with("/api/admin/tex-cache/") && path.ends_with("/tex"))
            || path == "/api/admin/note");
    let mutation = method != axum::http::Method::GET && path.starts_with("/api/");
    if !is_note && !mutation {
        return next.run(request).await;
    }
    let (mut parts, body) = request.into_parts();
    let user = CurrentUser::from_request_parts(&mut parts, &state)
        .await
        .ok();
    let target = if path == "/api/admin/note" {
        let mut ids = vec![];
        let parsed = reqwest::Url::parse(&format!(
            "http://localhost/?{}",
            parts.uri.query().unwrap_or("")
        ))
        .ok();
        for (k, v) in parsed.as_ref().into_iter().flat_map(|u| u.query_pairs()) {
            if matches!(k.as_ref(), "owner" | "course" | "lesson" | "job" | "format")
                && v.len() <= 160
            {
                ids.push(format!("{k}={v}"));
            }
        }
        format!("{path}:{}", ids.join(";"))
    } else if path == "/ta/action" {
        path.clone()
    } else {
        format!("{method} {}", path.chars().take(512).collect::<String>())
    };
    let response = next.run(Request::from_parts(parts, body)).await;
    if let Some(user) = user {
        if response.status().is_success()
            && matches!(path.as_str(), "/api/admin/features" | "/api/admin/ta-ai")
        {
            return response;
        }
        let kind = if is_note {
            "note.read"
        } else if path.starts_with("/api/admin/") {
            "admin.change"
        } else if path.ends_with("/reprocess") {
            "notes.reprocess"
        } else if path == "/ta/action" {
            "ta.action"
        } else {
            "user.action"
        };
        if state
            .store
            .record_event(Some(user.id), kind, &target, response.status().as_u16())
            .await
            .is_err()
        {
            tracing::warn!("操作记录持久化失败");
        }
    }
    response
}

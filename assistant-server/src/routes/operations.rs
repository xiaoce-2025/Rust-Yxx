//! Authenticated feature flags and metadata-only access audit.
use crate::{app_state::AppState, auth::CurrentUser, error::ApiError};
use axum::{
    extract::{FromRequestParts, MatchedPath, Request, State},
    http::{HeaderValue, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde_json::{json, Map, Value};
use uuid::Uuid;

#[derive(Clone, Copy)]
pub(super) struct AuditId(pub i64);

#[derive(Clone, Copy)]
pub(super) struct AuditJob(pub Uuid);

pub(super) fn job_response(job: Uuid, response: impl IntoResponse) -> Response {
    let mut response = response.into_response();
    response.extensions_mut().insert(AuditJob(job));
    response
}

pub(super) fn submitted_note(response: Json<Value>) -> Response {
    match response.0["job_id"].as_str().and_then(|id| Uuid::parse_str(id).ok()) {
        Some(id) => job_response(id, response),
        None => response.into_response(),
    }
}

fn resource_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

fn audit_target(route: &str, path: &str, query: Option<&str>) -> (Map<String, Value>, Option<Uuid>) {
    let mut resources = Map::new();
    let mut subject = None;
    for (template, value) in route.split('/').zip(path.split('/')) {
        match template {
            ":id" if route.starts_with("/api/admin/users/") => {
                subject = Uuid::parse_str(value).ok();
            }
            ":id" => {
                if let Ok(id) = Uuid::parse_str(value) {
                    resources.insert("job_id".into(), json!(id));
                }
            }
            ":course_id" | ":video_id" | ":lesson_id" | ":key" | ":action" | ":kind"
                if resource_id(value) =>
            {
                resources.insert(template[1..].into(), json!(value));
            }
            _ => {}
        }
    }
    let allowed: &[&str] = match route {
        "/api/admin/note" => &["owner", "course", "lesson", "job", "format"],
        "/ta/attachment" => &["course", "column", "attempt", "file"],
        _ => &[],
    };
    if !allowed.is_empty() {
        let parsed = reqwest::Url::parse(&format!("http://localhost/?{}", query.unwrap_or(""))).ok();
        for (name, value) in parsed.iter().flat_map(|uri| uri.query_pairs()) {
            if !allowed.contains(&name.as_ref()) || !resource_id(&value) {
                continue;
            }
            match name.as_ref() {
                "owner" => subject = Uuid::parse_str(&value).ok(),
                "job" => {
                    if let Ok(id) = Uuid::parse_str(&value) {
                        resources.insert("job_id".into(), json!(id));
                    }
                }
                "format" if !matches!(value.as_ref(), "tex" | "pdf") => {}
                _ => { resources.insert(name.into_owned(), json!(value)); }
            }
        }
    }
    (resources, subject)
}
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
    let Some(route) = request.extensions().get::<MatchedPath>().map(|path| path.as_str().to_owned()) else {
        return next.run(request).await;
    };
    let kind = match (&method, route.as_str()) {
        (&Method::POST, "/api/auth/iaaa/qr/poll" | "/api/teaching-session/qr/poll") => None,
        (&Method::POST, "/api/auth/login" | "/api/auth/iaaa" | "/api/auth/register"
            | "/api/auth/iaaa/qr/start" | "/api/auth/iaaa/qr/cancel")
        | (&Method::GET, "/auth/sso/callback") => Some("auth.request"),
        (&Method::GET | &Method::HEAD, path) if path.ends_with("/download-pdf")
            || path.ends_with("/download-tex")
            || matches!(path, "/api/pkuvideo/jobs/:id/pdf" | "/api/pkuvideo/jobs/:id/tex"
                | "/api/admin/note" | "/api/admin/tex-cache/:key/tex") => Some("note.read"),
        (&Method::GET, "/ta/attachment") => Some("ta.attachment"),
        (&Method::GET, "/api/admin/users/:id/note-credits/ledger") => Some("admin.read"),
        (&Method::POST, "/ta/action") => Some("ta.action"),
        (&Method::POST | &Method::PUT | &Method::PATCH | &Method::DELETE, path)
            if path.starts_with("/api/admin/") => Some("admin.change"),
        (&Method::POST, path) if path.ends_with("/reprocess") => Some("notes.reprocess"),
        (&Method::POST | &Method::PUT | &Method::PATCH | &Method::DELETE, path)
            if path.starts_with("/api/") => Some("user.action"),
        _ => None,
    };
    let Some(kind) = kind else { return next.run(request).await; };
    let (mut parts, body) = request.into_parts();
    // Public authentication and the password-authenticated submit route establish
    // their own identity inside the handler.
    let password_submit = route == "/api/pkuvideo/submit";
    let actor = if kind == "auth.request" || password_submit {
        None
    } else {
        CurrentUser::from_request_parts(&mut parts, &state).await.ok().map(|user| user.id)
    };
    if actor.is_none() && kind != "auth.request" && !password_submit {
        return next.run(Request::from_parts(parts, body)).await;
    }
    let (resources, subject) = audit_target(&route, parts.uri.path(), parts.uri.query());
    let target = format!("{method} {route}");
    let id = match state.store.begin_audit(actor, subject, kind, &target, &json!(resources)).await {
        Ok(id) => id,
        Err(error) => {
            tracing::error!(%error, %target, "操作记录写入失败，请求未执行");
            return (StatusCode::SERVICE_UNAVAILABLE,
                [(axum::http::header::CACHE_CONTROL, "no-store")],
                Json(json!({"error":"操作记录暂不可用，请稍后重试"}))).into_response();
        }
    };
    parts.extensions.insert(AuditId(id));
    let started = std::time::Instant::now();
    let mut response = next.run(Request::from_parts(parts, body)).await;
    let mut detail = json!({"duration_ms": started.elapsed().as_millis() as u64});
    if let Some(job) = response.extensions().get::<AuditJob>() {
        detail["result_job_id"] = json!(job.0);
    }
    if let Err(error) = state.store.finish_audit(id, response.status().as_u16(), &detail).await {
        tracing::error!(%error, audit_id=id, status=%response.status(), "操作结果写入失败，请按记录编号核对");
    }
    response.headers_mut().insert("x-audit-id", HeaderValue::from(id));
    response
}

use crate::{
    app_state::AppState,
    auth::CurrentUser,
    error::ApiError,
    services::{
        iaaa_qr::{self, Poll},
        pkulogin, teaching_session,
    },
};
use axum::{
    extract::State,
    http::{
        header::{CACHE_CONTROL, SET_COOKIE},
        HeaderMap,
    },
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::atomic::Ordering;
use std::time::Instant;
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Ticket {
    id: String,
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/auth/iaaa/qr/start", post(start_auth))
        .route("/api/auth/iaaa/qr/poll", post(poll_auth))
        .route("/api/auth/iaaa/qr/cancel", post(cancel_auth))
        .route("/api/teaching-session/qr/start", post(start_teaching))
        .route("/api/teaching-session/qr/poll", post(poll_teaching))
        .route("/api/teaching-session/qr/cancel", post(cancel_teaching))
}

fn cookie_name(owner: Option<Uuid>) -> &'static str {
    if owner.is_some() {
        "iaaa_qr_teaching"
    } else {
        "iaaa_qr_auth"
    }
}
fn cookie_id(headers: &HeaderMap, owner: Option<Uuid>) -> Option<&str> {
    headers
        .get(axum::http::header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|part| part.trim().split_once('='))
        .find(|(name, _)| *name == cookie_name(owner))
        .map(|(_, value)| value)
}
fn check_teaching(headers: &HeaderMap, user: Uuid) -> Result<(), ApiError> {
    if headers.get("x-teaching-csrf").and_then(|v| v.to_str().ok())
        != Some(super::teaching_session::csrf(headers, user).as_str())
    {
        return Err(ApiError::Forbidden("页面已过期，请刷新后重试".into()));
    }
    Ok(())
}

async fn start_auth(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(_): Json<Empty>,
) -> Result<Response, ApiError> {
    start(state, headers, None).await
}
async fn start_teaching(
    State(state): State<AppState>,
    user: CurrentUser,
    headers: HeaderMap,
    Json(_): Json<Empty>,
) -> Result<Response, ApiError> {
    check_teaching(&headers, user.id)?;
    start(state, headers, Some(user.id)).await
}
async fn start(
    state: AppState,
    headers: HeaderMap,
    owner: Option<Uuid>,
) -> Result<Response, ApiError> {
    {
        let mut transactions = state.iaaa_qr.lock().await;
        transactions.retain(|_, t| {
            if t.created.elapsed() >= iaaa_qr::LIFETIME {
                t.active.store(false, Ordering::SeqCst);
                false
            } else {
                true
            }
        });
        if let Some(old) = cookie_id(&headers, owner) {
            if transactions.get(old).is_some_and(|t| t.owner == owner) {
                if let Some(t) = transactions.remove(old) {
                    t.active.store(false, Ordering::SeqCst);
                }
            }
        }
        if transactions.len() >= 256 {
            return Err(ApiError::BadRequest("扫码登录繁忙，请稍后重试".into()));
        }
    }
    let (transaction, image) = tokio::task::spawn_blocking(move || iaaa_qr::start(owner))
        .await
        .map_err(|_| ApiError::Internal("创建扫码会话失败".into()))?
        .map_err(|_| ApiError::BadRequest("获取 IAAA 二维码失败，请刷新重试".into()))?;
    let id = Uuid::new_v4().to_string();
    {
        let mut transactions = state.iaaa_qr.lock().await;
        if transactions.len() >= 256 {
            return Err(ApiError::BadRequest("扫码登录繁忙，请稍后重试".into()));
        }
        transactions.insert(id.clone(), transaction);
    }
    let cleanup = state.iaaa_qr.clone();
    let expired_id = id.clone();
    tokio::spawn(async move {
        tokio::time::sleep(iaaa_qr::LIFETIME).await;
        if let Some(t) = cleanup.lock().await.remove(&expired_id) {
            t.active.store(false, Ordering::SeqCst);
        }
    });
    let path = if owner.is_some() {
        "/api/teaching-session/qr"
    } else {
        "/api/auth/iaaa/qr"
    };
    let secure = if headers
        .get("x-forwarded-proto")
        .is_some_and(|v| v == "https")
        || headers
            .get("origin")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("https://"))
    {
        "; Secure"
    } else {
        ""
    };
    let cookie = format!(
        "{}={id}; Path={path}; HttpOnly; SameSite=Strict; Max-Age=183{secure}",
        cookie_name(owner)
    );
    Ok((
        [(CACHE_CONTROL, "no-store"), (SET_COOKIE, cookie.as_str())],
        Json(json!({"id":id,"image":image,"expires_in":180})),
    )
        .into_response())
}

async fn poll_auth(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(ticket): Json<Ticket>,
) -> Result<Response, ApiError> {
    poll(state, headers, None, ticket).await
}
async fn poll_teaching(
    State(state): State<AppState>,
    user: CurrentUser,
    headers: HeaderMap,
    Json(ticket): Json<Ticket>,
) -> Result<Response, ApiError> {
    check_teaching(&headers, user.id)?;
    poll(state, headers, Some(user.id), ticket).await
}
async fn poll(
    state: AppState,
    headers: HeaderMap,
    owner: Option<Uuid>,
    ticket: Ticket,
) -> Result<Response, ApiError> {
    if cookie_id(&headers, owner) != Some(ticket.id.as_str()) {
        return Err(ApiError::Forbidden("扫码会话不匹配，请刷新二维码".into()));
    }
    let transaction = {
        let mut transactions = state.iaaa_qr.lock().await;
        let valid = transactions
            .get(&ticket.id)
            .is_some_and(|t| t.owner == owner && t.created.elapsed() < iaaa_qr::LIFETIME);
        if !valid {
            return Err(ApiError::BadRequest(
                "二维码已过期，请刷新后重新扫码".into(),
            ));
        }
        let transaction = transactions.get_mut(&ticket.id).unwrap();
        if transaction.last_poll.elapsed().as_secs() < 3
            || transaction.polling.load(Ordering::SeqCst)
        {
            return Ok(pending());
        }
        transaction.last_poll = Instant::now();
        transaction.polling.store(true, Ordering::SeqCst);
        transaction.clone()
    };
    let (transaction, result) = tokio::task::spawn_blocking(move || {
        let result = iaaa_qr::poll(&transaction);
        (transaction, result)
    })
    .await
    .map_err(|_| ApiError::Internal("扫码状态查询失败，请刷新二维码".into()))?;
    if !transaction.active.load(Ordering::SeqCst)
        || transaction.created.elapsed() >= iaaa_qr::LIFETIME
    {
        return Err(ApiError::BadRequest(
            "二维码已过期或已取消，请刷新后重新扫码".into(),
        ));
    }
    // Only pending results retain a usable transaction; terminal results consume it once.
    if !matches!(&result, Ok(Poll::Pending)) {
        if !transaction.active.swap(false, Ordering::SeqCst) {
            return Err(ApiError::BadRequest("扫码会话已结束".into()));
        }
        state.iaaa_qr.lock().await.remove(&ticket.id);
    } else {
        transaction.polling.store(false, Ordering::SeqCst);
    }
    match result.map_err(|_| ApiError::BadRequest("扫码状态查询失败，请刷新二维码".into()))?
    {
        Poll::Pending => {
            if transaction.created.elapsed() >= iaaa_qr::LIFETIME {
                return Err(ApiError::BadRequest(
                    "二维码已过期，请刷新后重新扫码".into(),
                ));
            }
            Ok(pending())
        }
        Poll::Stopped(message) => Ok((
            [(CACHE_CONTROL, "no-store")],
            Json(json!({"status":"stopped","message":message})),
        )
            .into_response()),
        Poll::Accepted(token) => {
            let session = transaction.session;
            let (session, username) = tokio::task::spawn_blocking(move || {
                let username = pkulogin::complete_qr_login(&session, &token);
                (session, username)
            })
            .await
            .map_err(|_| ApiError::Internal("完成扫码认证失败，请重新扫码".into()))?;
            let username = username.map_err(|_| {
                ApiError::BadRequest("无法确认扫码账号或教学网登录失败，请重新扫码".into())
            })?;
            if let Some(owner) = owner {
                let refresh_lock = super::assignments::refresh_lock(&state, owner).await;
                let _refresh_guard = refresh_lock.lock().await;
                super::assignments::clear_snapshot(&state.store, owner).await?;
                state
                    .login_cache
                    .insert(&teaching_session::cache_key(owner), session)
                    .await;
                Ok((
                    [(CACHE_CONTROL, "no-store")],
                    Json(json!({"status":"success"})),
                )
                    .into_response())
            } else {
                // System login deliberately discards the temporary teaching session.
                let authenticated =
                    super::auth::complete_iaaa_login(&state, &headers, &username).await?;
                let mut response = (
                    [(CACHE_CONTROL, "no-store")],
                    Json(json!({"status":"success"})),
                )
                    .into_response();
                if let Some(cookie) = authenticated.headers().get(SET_COOKIE) {
                    response.headers_mut().insert(SET_COOKIE, cookie.clone());
                }
                Ok(response)
            }
        }
    }
}
fn pending() -> Response {
    (
        [(CACHE_CONTROL, "no-store")],
        Json(json!({"status":"pending"})),
    )
        .into_response()
}

async fn cancel_auth(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(ticket): Json<Ticket>,
) -> Result<Response, ApiError> {
    cancel(state, headers, None, ticket).await
}
async fn cancel_teaching(
    State(state): State<AppState>,
    user: CurrentUser,
    headers: HeaderMap,
    Json(ticket): Json<Ticket>,
) -> Result<Response, ApiError> {
    check_teaching(&headers, user.id)?;
    cancel(state, headers, Some(user.id), ticket).await
}
async fn cancel(
    state: AppState,
    headers: HeaderMap,
    owner: Option<Uuid>,
    ticket: Ticket,
) -> Result<Response, ApiError> {
    if cookie_id(&headers, owner) != Some(ticket.id.as_str()) {
        return Err(ApiError::Forbidden("扫码会话不匹配".into()));
    }
    let mut transactions = state.iaaa_qr.lock().await;
    if transactions
        .get(&ticket.id)
        .is_some_and(|t| t.owner == owner)
    {
        if let Some(t) = transactions.remove(&ticket.id) {
            t.active.store(false, Ordering::SeqCst);
        }
    }
    Ok((
        [(CACHE_CONTROL, "no-store")],
        Json(json!({"status":"cancelled"})),
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_binding_and_teaching_csrf_are_separate_from_system_login() {
        let owner = Uuid::new_v4();
        let mut headers = HeaderMap::new();
        headers.insert(
            "cookie",
            "assistant_token=local-session; iaaa_qr_auth=system; iaaa_qr_teaching=teaching"
                .parse()
                .unwrap(),
        );
        assert_eq!(cookie_id(&headers, None), Some("system"));
        assert_eq!(cookie_id(&headers, Some(owner)), Some("teaching"));
        assert!(check_teaching(&headers, owner).is_err());
        let csrf = super::super::teaching_session::csrf(&headers, owner);
        headers.insert("x-teaching-csrf", csrf.parse().unwrap());
        assert!(check_teaching(&headers, owner).is_ok());
        assert!(check_teaching(&headers, Uuid::new_v4()).is_err());
        headers.remove("cookie");
        assert_eq!(cookie_id(&headers, None), None);
        assert!(check_teaching(&headers, owner).is_err());
    }
}

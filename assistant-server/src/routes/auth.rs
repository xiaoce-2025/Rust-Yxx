use axum::{
    extract::State,
    http::{header::SET_COOKIE, HeaderMap, HeaderValue},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    app_state::AppState,
    auth::{extract_session_token, hash_password, session_expiry, verify_password, CurrentUser},
    error::ApiError,
    store::{FileUser, FileUserSession},
};

#[derive(Debug, Deserialize)]
struct RegisterRequest {
    username: String,
    email: String,
    password: String,
}

#[derive(Debug, Deserialize)]
struct LoginRequest {
    email: String,
    password: String,
}

#[derive(Deserialize)]
struct IaaaLoginRequest {
    username: String,
    password: String,
}

#[derive(Debug, Serialize)]
struct AuthResponse {
    token: String,
    user_id: Uuid,
    username: String,
    email: String,
    expires_at: chrono::DateTime<Utc>,
}

#[derive(Debug, Serialize)]
struct SimpleResponse {
    success: bool,
}

#[derive(Debug, Serialize)]
struct CurrentUserResponse {
    user_id: Uuid,
    username: String,
    email: String,
    storage_quota: i32,
    processing_quota: i32,
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/auth/me", get(me))
        .route("/api/auth/register", post(register))
        .route("/api/auth/login", post(login))
        .route("/api/auth/iaaa", post(iaaa_login))
        .route("/api/auth/logout", post(logout))
}

async fn me(State(state): State<AppState>, current_user: CurrentUser) -> Result<Json<CurrentUserResponse>, ApiError> {
    let user = state.store.get_user(current_user.id).await.ok_or(ApiError::Unauthorized)?;
    Ok(Json(CurrentUserResponse {
        user_id: current_user.id,
        username: current_user.username,
        email: current_user.email,
        storage_quota: user.storage_quota,
        processing_quota: user.processing_quota,
    }))
}

async fn register(State(state): State<AppState>, Json(req): Json<RegisterRequest>) -> Result<axum::response::Response, ApiError> {
    if req.email.trim().is_empty() || req.password.trim().is_empty() || req.username.trim().is_empty() {
        return Err(ApiError::BadRequest("username/email/password 不能为空".to_string()));
    }

    if state.store.get_user_by_email(req.email.trim()).await.is_some() {
        return Err(ApiError::Conflict("邮箱已存在".to_string()));
    }

    if state.store.get_user_by_username(req.username.trim()).await.is_some() {
        return Err(ApiError::Conflict("用户名已存在".to_string()));
    }

    if !state.store.is_email_in_whitelist(req.email.trim()).await {
        return Err(ApiError::Forbidden("注册邮箱校验失败！请使用学号邮箱进行注册".to_string()));
    }

    let password_hash = hash_password(req.password.trim())?;
    let user_id = Uuid::new_v4();
    let now = Utc::now();

    let user = FileUser {
        processing_priority: 5,
        id: user_id,
        username: req.username.trim().to_string(),
        email: req.email.trim().to_string(),
        password_hash,
        is_active: true,
        created_at: now,
        updated_at: now,
        storage_quota: 3,
        processing_quota: 1,
        iaaa_encrypted_password: None,
        iaaa_public_key: None,
    };

    state.store.insert_user(user.clone()).await
        .map_err(|e| ApiError::Internal(format!("Failed to insert user: {}", e)))?;

    let session_id = Uuid::new_v4();
    let expires_at = session_expiry(state.settings.security.session_ttl_hours);

    let session = FileUserSession {
        session_id,
        user_id,
        expires_at,
        user_agent: None,
        ip_address: None,
        created_at: Utc::now(),
    };

    state.store.insert_session(session).await
        .map_err(|e| ApiError::Internal(format!("Failed to insert session: {}", e)))?;

    let resp = AuthResponse {
        token: session_id.to_string(),
        user_id: user.id,
        username: user.username,
        email: user.email,
        expires_at,
    };

    let cookie = build_auth_cookie(&resp.token, state.settings.security.session_ttl_hours);

    Ok(([ (SET_COOKIE, cookie) ], Json(resp)).into_response())
}

async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<LoginRequest>,
) -> Result<axum::response::Response, ApiError> {
    let user = state.store.get_user_by_email(req.email.trim()).await
        .ok_or(ApiError::Unauthorized)?;

    if !user.is_active {
        return Err(ApiError::Forbidden("用户未激活".to_string()));
    }

    if !verify_password(req.password.trim(), &user.password_hash)? {
        return Err(ApiError::Unauthorized);
    }

    let session_id = Uuid::new_v4();
    let expires_at = session_expiry(state.settings.security.session_ttl_hours);
    let user_agent = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);

    let session = FileUserSession {
        session_id,
        user_id: user.id,
        expires_at,
        user_agent,
        ip_address: None,
        created_at: Utc::now(),
    };

    state.store.insert_session(session).await
        .map_err(|e| ApiError::Internal(format!("Failed to insert session: {}", e)))?;

    let resp = AuthResponse {
        token: session_id.to_string(),
        user_id: user.id,
        username: user.username,
        email: user.email,
        expires_at,
    };

    let cookie = build_auth_cookie(&resp.token, state.settings.security.session_ttl_hours);

    Ok(([ (SET_COOKIE, cookie) ], Json(resp)).into_response())
}

fn iaaa_email(username: &str) -> Result<String, ApiError> {
    if username.len() != 10 || !username.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ApiError::BadRequest("请输入 10 位学号".into()));
    }
    Ok(format!("{username}@stu.pku.edu.cn"))
}

async fn iaaa_login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<IaaaLoginRequest>,
) -> Result<axum::response::Response, ApiError> {
    let username = req.username.trim();
    let email = iaaa_email(username)?;
    if req.password.is_empty() || req.password.len() > 1024 {
        return Err(ApiError::BadRequest("请输入有效的 IAAA 密码".into()));
    }
    crate::services::iaaa_login::verify(username, &req.password).await?;

    // A random local password prevents the university password from becoming the
    // assistant password. Existing passwords and bindings are left intact.
    let now = Utc::now();
    let candidate = FileUser {
        id: Uuid::new_v4(), username: username.to_string(), email,
        password_hash: hash_password(&Uuid::new_v4().to_string())?,
        is_active: true, created_at: now, updated_at: now,
        processing_priority: 5, storage_quota: 3, processing_quota: 1,
        iaaa_encrypted_password: None, iaaa_public_key: None,
    };
    let user = state.store.resolve_iaaa_user(candidate).await
        .map_err(|_| ApiError::Internal("创建 IAAA 账户失败，请稍后重试".into()))?
        .ok_or_else(|| ApiError::Forbidden("该学号邮箱不在注册白名单中，请联系管理员".into()))?;
    if !user.is_active {
        return Err(ApiError::Forbidden("用户未激活".into()));
    }
    let session_id = Uuid::new_v4();
    let expires_at = session_expiry(state.settings.security.session_ttl_hours);
    state.store.insert_session(FileUserSession {
        session_id, user_id: user.id, expires_at, created_at: Utc::now(),
        user_agent: headers.get("user-agent").and_then(|v| v.to_str().ok()).map(ToString::to_string),
        ip_address: None,
    }).await.map_err(|_| ApiError::Internal("创建登录会话失败，请稍后重试".into()))?;
    let resp = AuthResponse {
        token: session_id.to_string(), user_id: user.id,
        username: user.username, email: user.email, expires_at,
    };
    let cookie = build_auth_cookie(&resp.token, state.settings.security.session_ttl_hours);
    Ok(([(SET_COOKIE, cookie)], Json(resp)).into_response())
}

async fn logout(
    State(state): State<AppState>,
    _current_user: CurrentUser,
    headers: HeaderMap,
) -> Result<axum::response::Response, ApiError> {
    let token = extract_session_token(&headers).ok_or(ApiError::Unauthorized)?;
    let session_id = Uuid::parse_str(&token).map_err(|_| ApiError::Unauthorized)?;

    state.store.delete_session(session_id).await
        .map_err(|e| ApiError::Internal(format!("Failed to delete session: {}", e)))?;

    let clear_cookie = HeaderValue::from_static("assistant_token=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0");

    Ok(([ (SET_COOKIE, clear_cookie) ], Json(SimpleResponse { success: true })).into_response())
}

fn build_auth_cookie(token: &str, ttl_hours: i64) -> HeaderValue {
    let max_age_seconds = (ttl_hours.max(1) * 3600) as i64;
    let cookie = format!(
        "assistant_token={}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
        token, max_age_seconds
    );
    HeaderValue::from_str(&cookie).unwrap_or_else(|_| HeaderValue::from_static("assistant_token=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0"))
}
#[cfg(test)]
mod iaaa_tests {
    use super::*;
    use crate::store::FileStore;

    fn candidate() -> FileUser {
        FileUser {
            id: Uuid::new_v4(), username: "2400012345".into(),
            email: "2400012345@stu.pku.edu.cn".into(), password_hash: "original-hash".into(),
            is_active: true, created_at: Utc::now(), updated_at: Utc::now(),
            processing_priority: 5, storage_quota: 3, processing_quota: 1,
            iaaa_encrypted_password: None, iaaa_public_key: None,
        }
    }

    #[test]
    fn student_identity_cannot_override_email_domain() {
        assert_eq!(iaaa_email("2400012345").unwrap(), "2400012345@stu.pku.edu.cn");
        for invalid in ["", "123", "2400012345@stu.pku.edu.cn", "２４０００１２３４５", "240001234a"] {
            assert!(iaaa_email(invalid).is_err());
        }
    }

    #[tokio::test]
    async fn iaaa_registration_requires_whitelist_and_reuses_existing_identity() {
        let dir = tempdir::TempDir::new("iaaa-users").unwrap();
        let store = FileStore::new(dir.path().into()).await.unwrap();
        assert!(store.resolve_iaaa_user(candidate()).await.unwrap().is_none());
        assert!(store.get_user_by_email(&candidate().email).await.is_none());
        std::fs::write(dir.path().join("whitelist.json"),
            r#"{"allowed_emails":["2400012345@stu.pku.edu.cn"]}"#).unwrap();
        store.reload_whitelist().await.unwrap();
        let (first, second) = tokio::join!(store.resolve_iaaa_user(candidate()), store.resolve_iaaa_user(candidate()));
        let first = first.unwrap().unwrap();
        assert_eq!(first.id, second.unwrap().unwrap().id);
        assert_eq!(first.storage_quota, 3);
        assert_eq!(first.processing_quota, 1);
        assert!(first.iaaa_encrypted_password.is_none());
        std::fs::write(dir.path().join("whitelist.json"), r#"{"allowed_emails":[]}"#).unwrap();
        store.reload_whitelist().await.unwrap();
        let mut replacement = candidate();
        replacement.password_hash = "replacement-hash".into();
        let resolved = store.resolve_iaaa_user(replacement).await.unwrap().unwrap();
        assert_eq!(resolved.id, first.id);
        assert_eq!(resolved.password_hash, "original-hash");
        let reopened = FileStore::new(dir.path().into()).await.unwrap();
        assert_eq!(reopened.get_user_by_email(&first.email).await.unwrap().id, first.id);
    }

    #[tokio::test]
    async fn iaaa_resolution_preserves_disabled_accounts() {
        let dir = tempdir::TempDir::new("iaaa-disabled").unwrap();
        let store = FileStore::new(dir.path().into()).await.unwrap();
        let mut user = candidate();
        user.is_active = false;
        store.insert_user(user.clone()).await.unwrap();
        let resolved = store.resolve_iaaa_user(candidate()).await.unwrap().unwrap();
        assert_eq!(resolved.id, user.id);
        assert!(!resolved.is_active);
    }
}

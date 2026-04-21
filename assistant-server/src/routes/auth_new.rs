use axum::{extract::State, http::HeaderMap, routing::{get, post}, Json, Router};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    app_state::AppState,
    auth::{hash_password, session_expiry, verify_password, CurrentUser},
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
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/auth/me", get(me))
        .route("/api/auth/register", post(register))
        .route("/api/auth/login", post(login))
        .route("/api/auth/logout", post(logout))
}

async fn me(current_user: CurrentUser) -> Result<Json<CurrentUserResponse>, ApiError> {
    Ok(Json(CurrentUserResponse {
        user_id: current_user.id,
        username: current_user.username,
        email: current_user.email,
    }))
}

async fn register(State(state): State<AppState>, Json(req): Json<RegisterRequest>) -> Result<Json<AuthResponse>, ApiError> {
    if req.email.trim().is_empty() || req.password.trim().is_empty() || req.username.trim().is_empty() {
        return Err(ApiError::BadRequest("username/email/password 不能为空".to_string()));
    }

    if state.store.get_user_by_email(req.email.trim()).await.is_some() {
        return Err(ApiError::Conflict("邮箱已存在".to_string()));
    }

    let password_hash = hash_password(req.password.trim())?;
    let user_id = Uuid::new_v4();
    let now = Utc::now();

    let user = FileUser {
        id: user_id,
        username: req.username.trim().to_string(),
        email: req.email.trim().to_string(),
        password_hash,
        is_active: true,
        created_at: now,
        updated_at: now,
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

    Ok(Json(AuthResponse {
        token: session_id.to_string(),
        user_id: user.id,
        username: user.username,
        email: user.email,
        expires_at,
    }))
}

async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<LoginRequest>,
) -> Result<Json<AuthResponse>, ApiError> {
    let user = state.store.get_user_by_email(req.email.trim()).await
        .ok_or(ApiError::Unauthorized)?;

    if !user.is_active {
        return Err(ApiError::Forbidden);
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

    Ok(Json(AuthResponse {
        token: session_id.to_string(),
        user_id: user.id,
        username: user.username,
        email: user.email,
        expires_at,
    }))
}

async fn logout(
    State(state): State<AppState>,
    current_user: CurrentUser,
    headers: HeaderMap,
) -> Result<Json<SimpleResponse>, ApiError> {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let token = auth.strip_prefix("Bearer ").unwrap_or_default();
    let session_id = Uuid::parse_str(token).map_err(|_| ApiError::Unauthorized)?;

    state.store.delete_session(session_id).await
        .map_err(|e| ApiError::Internal(format!("Failed to delete session: {}", e)))?;

    Ok(Json(SimpleResponse { success: true }))
}

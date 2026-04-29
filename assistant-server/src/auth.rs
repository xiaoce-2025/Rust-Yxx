use std::str::FromStr;

use argon2::{password_hash::SaltString, Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use axum::{
    async_trait,
    extract::{FromRef, FromRequestParts},
    http::{header::AUTHORIZATION, HeaderMap, request::Parts},
};
use chrono::{Duration, Utc};
use rand::rngs::OsRng;
use uuid::Uuid;

use crate::{app_state::AppState, error::ApiError};

#[derive(Debug, Clone)]
pub struct CurrentUser {
    pub id: Uuid,
    pub username: String,
    pub email: String,
}

#[async_trait]
impl<S> FromRequestParts<S> for CurrentUser
where
    AppState: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let app_state = AppState::from_ref(state);

        let token = extract_session_token(&parts.headers).ok_or(ApiError::Unauthorized)?;

        let session_id = Uuid::from_str(&token).map_err(|_| ApiError::Unauthorized)?;

        let session = app_state.store
            .get_session(session_id)
            .await
            .ok_or(ApiError::Unauthorized)?;

        if session.expires_at < Utc::now() {
            return Err(ApiError::Unauthorized);
        }

        let user = app_state.store
            .get_user(session.user_id)
            .await
            .ok_or(ApiError::Unauthorized)?;

        if !user.is_active {
            return Err(ApiError::Unauthorized);
        }

        Ok(Self {
            id: user.id,
            username: user.username,
            email: user.email,
        })
    }
}

pub fn extract_session_token(headers: &HeaderMap) -> Option<String> {
    if let Some(auth) = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        if let Some(token) = auth.strip_prefix("Bearer ") {
            if !token.trim().is_empty() {
                return Some(token.trim().to_string());
            }
        }
    }

    let cookie_header = headers.get("cookie").and_then(|v| v.to_str().ok())?;
    for segment in cookie_header.split(';') {
        let mut kv = segment.trim().splitn(2, '=');
        let key = kv.next().unwrap_or_default().trim();
        let value = kv.next().unwrap_or_default().trim();
        if key == "assistant_token" && !value.is_empty() {
            return Some(value.to_string());
        }
    }

    None
}

pub fn hash_password(password: &str) -> Result<String, ApiError> {
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    argon2
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| ApiError::Internal(format!("password hash failed: {e}")))
}

pub fn verify_password(password: &str, password_hash: &str) -> Result<bool, ApiError> {
    let parsed_hash = PasswordHash::new(password_hash)
        .map_err(|e| ApiError::Internal(format!("invalid password hash: {e}")))?;
    Ok(Argon2::default()
        .verify_password(password.as_bytes(), &parsed_hash)
        .is_ok())
}

pub fn session_expiry(ttl_hours: i64) -> chrono::DateTime<Utc> {
    Utc::now() + Duration::hours(ttl_hours)
}

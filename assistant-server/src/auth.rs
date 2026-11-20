use std::str::FromStr;

use argon2::{password_hash::SaltString, Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use axum::{
    async_trait,
    extract::{FromRef, FromRequestParts},
    http::{header::AUTHORIZATION, HeaderMap, HeaderValue, request::Parts},
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

#[derive(Clone)]
struct RequestIdentity(CurrentUser);

#[async_trait]
impl<S> FromRequestParts<S> for CurrentUser
where
    AppState: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        if let Some(identity) = parts.extensions.get::<RequestIdentity>() {
            return Ok(identity.0.clone());
        }
        let app_state = AppState::from_ref(state);

        let token = extract_session_token(&parts.headers).ok_or(ApiError::Unauthorized)?;

        let session_id = Uuid::from_str(&token).map_err(|_| ApiError::Unauthorized)?;

        match app_state.store.authenticated_user(session_id).await {
            Ok(Some(user)) => {
                parts.extensions.insert(RequestIdentity(user.clone()));
                Ok(user)
            }
            Ok(None) => Err(ApiError::Unauthorized),
            Err(error) => {
                tracing::error!(%error, "读取登录会话失败");
                Err(ApiError::Internal("读取登录会话失败".into()))
            }
        }

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

// The browser owns the session lifetime; the server expiry remains an upper bound.
pub fn browser_session_cookie(token: &str, secure: bool) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "assistant_token={token}; Path=/; HttpOnly; SameSite=Lax{}",
        if secure { "; Secure" } else { "" }
    )).expect("generated session token")
}

#[cfg(test)]
mod cookie_tests {
    use super::*;
    #[test]
    fn application_login_cookie_is_not_persistent() {
        for secure in [false, true] {
            let cookie = browser_session_cookie(&Uuid::new_v4().to_string(), secure);
            let value = cookie.to_str().unwrap();
            assert!(!value.contains("Max-Age="));
            assert!(!value.contains("Expires="));
            assert!(value.contains("HttpOnly; SameSite=Lax"));
            assert_eq!(value.contains("; Secure"), secure);
        }
    }
}

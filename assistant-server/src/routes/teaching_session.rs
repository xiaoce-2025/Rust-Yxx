use crate::{
    app_state::AppState,
    auth::CurrentUser,
    error::ApiError,
    services::{pkulogin, teaching_session},
};
use axum::{
    extract::State,
    http::{header::CACHE_CONTROL, HeaderMap},
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/teaching-session", get(status).post(login))
}

pub(super) fn csrf(headers: &HeaderMap, user: uuid::Uuid) -> String {
    let token = crate::auth::extract_session_token(headers).unwrap_or_default();
    format!(
        "{:x}",
        Sha256::digest(format!("teaching-login-v1:{user}:{token}"))
    )
}

async fn status(
    State(state): State<AppState>,
    user: CurrentUser,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    let stored = state
        .store
        .get_user(user.id)
        .await
        .ok_or(ApiError::Unauthorized)?;
    Ok((
        [(CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({
            "is_bound": stored.iaaa_encrypted_password.is_some(),
            "has_session": teaching_session::cached(&state, &stored).await.is_some(),
        })),
    ))
}

#[derive(Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

async fn login(
    State(state): State<AppState>,
    user: CurrentUser,
    headers: HeaderMap,
    Json(req): Json<LoginRequest>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    if headers.get("x-teaching-csrf").and_then(|v| v.to_str().ok())
        != Some(csrf(&headers, user.id).as_str())
    {
        return Err(ApiError::BadRequest("页面已过期，请刷新后重试".into()));
    }
    let username = req.username.trim();
    if username.len() != 10 || !username.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ApiError::BadRequest("请输入 10 位学号".into()));
    }
    if req.password.is_empty() || req.password.len() > 1024 {
        return Err(ApiError::BadRequest("请输入有效的 IAAA 密码".into()));
    }
    let key = teaching_session::cache_key(user.id);
    let refresh_lock = super::assignments::refresh_lock(&state, user.id).await;
    let _refresh_guard = refresh_lock.lock().await;
    let lock = state.login_cache.login_lock(&key).await;
    let _guard = lock.lock().await;
    // Verify fresh credentials without saving the password or its ciphertext.
    let (session, _, _) = pkulogin::login_iaaa(username, pkulogin::LoginPassword::Plaintext(req.password))
        .await
        .map_err(|error| ApiError::BadRequest(if error.contains("次数过多") {
            crate::services::iaaa_login::LOGIN_LIMIT_MESSAGE.into()
        } else { "教学网登录失败，请检查账号密码或稍后重试".into() }))?;
    super::assignments::clear_snapshot(&state.store, user.id).await?;
    state.login_cache.insert(&key, session).await;
    Ok((
        [(CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({"success":true})),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Settings,
        services::login_cache::LoginCache,
        store::{FileStore, FileUser, FileUserSession},
    };
    use chrono::{Duration, Utc};
    use std::sync::Arc;
    use uuid::Uuid;

    #[tokio::test]
    async fn temporary_cookie_survives_restart_is_user_scoped_and_never_binds_credentials() {
        let dir = tempdir::TempDir::new("teaching-once").unwrap();
        let settings: Settings = serde_json::from_value(serde_json::json!({
            "server":{"address":"127.0.0.1","port":0},
            "wechat":{"api_base_url":"","oauth_app_id":"","oauth_app_secret":"","redirect_uri":"","qr_bot_type":""},
            "security":{"session_ttl_hours":1},"cors":{"allowed_origins":[],"allow_credentials":false},
            "audioapi_paraformer":{},"llm_deepseek":{}
        })).unwrap();
        let store = Arc::new(FileStore::new(dir.path().join("data")).await.unwrap());
        let id = Uuid::new_v4();
        let user = FileUser {
            id,
            username: "local-account".into(),
            email: "local@example.invalid".into(),
            password_hash: "local-password-hash".into(),
            is_active: true,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            storage_quota: 3,
            processing_quota: 10,
            processing_priority: 5,
            iaaa_encrypted_password: None,
            iaaa_public_key: None,
            iaaa_key_version: None,
            iaaa_username: None,
        };
        store.insert_user(user.clone()).await.unwrap();
        let cache_dir = dir.path().join("teaching_sessions");
        let state = AppState::with_login_cache(
            settings.clone(),
            store.clone(),
            Arc::new(LoginCache::persistent(cache_dir.clone()).unwrap()),
        );
        assert!(teaching_session::login(&state, &user)
            .await
            .unwrap_err()
            .contains("尚未绑定教学网"));
        let cookie = dir.path().join("initial.cookies");
        std::fs::write(&cookie, "# Netscape HTTP Cookie File\n").unwrap();
        let mut session = pkulogin::PkuLoginResult::new(cookie, "test-agent".into());
        session.created_at = Utc::now() - Duration::days(365);
        let session = state
            .login_cache
            .insert(&teaching_session::cache_key(id), session)
            .await;
        let expected = session.cookie_file.clone();
        assert_eq!(
            teaching_session::login(&state, &user)
                .await
                .unwrap()
                .cookie_file,
            expected
        );
        let mut other = user.clone();
        other.id = Uuid::new_v4();
        assert!(teaching_session::cached(&state, &other).await.is_none());
        let stored = store.get_user(id).await.unwrap();
        assert!(stored.iaaa_encrypted_password.is_none() && stored.iaaa_public_key.is_none());
        assert_eq!(stored.password_hash, "local-password-hash");
        for item in std::fs::read_dir(&cache_dir).unwrap() {
            let path = item.unwrap().path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                let saved: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
                assert_eq!(saved["username"], teaching_session::cache_key(id));
                assert!(saved.get("password").is_none());
            }
        }
        drop(session);
        drop(state);
        let state = AppState::with_login_cache(
            settings,
            store.clone(),
            Arc::new(LoginCache::persistent(cache_dir).unwrap()),
        );
        assert_eq!(
            teaching_session::login(&state, &user)
                .await
                .unwrap()
                .cookie_file,
            expected
        );

        let token = Uuid::new_v4();
        store
            .insert_session(FileUserSession {
                session_id: token,
                user_id: id,
                expires_at: Utc::now() + Duration::hours(1),
                created_at: Utc::now(),
                user_agent: None,
                ip_address: None,
            })
            .await
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = routes().with_state(state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = reqwest::Client::new();
        assert_eq!(
            client
                .get(format!("{base}/api/teaching-session"))
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        let response = client
            .get(format!("{base}/api/teaching-session"))
            .header("cookie", format!("assistant_token={token}"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.headers()[CACHE_CONTROL], "no-store");
        let value: serde_json::Value = response.json().await.unwrap();
        assert_eq!(
            value,
            serde_json::json!({"is_bound":false,"has_session":true})
        );
        let request = serde_json::json!({"username":"2400012345","password":"never-store-this"});
        let rejected = client
            .post(format!("{base}/api/teaching-session"))
            .header("cookie", format!("assistant_token={token}"))
            .json(&request)
            .send()
            .await
            .unwrap();
        assert_eq!(rejected.status(), 400);
        let mut headers = HeaderMap::new();
        headers.insert(
            "cookie",
            format!("assistant_token={token}").parse().unwrap(),
        );
        let csrf_value = csrf(&headers, id);
        let invalid = client
            .post(format!("{base}/api/teaching-session"))
            .header("cookie", format!("assistant_token={token}"))
            .header("x-teaching-csrf", &csrf_value)
            .json(&serde_json::json!({"username":"invalid","password":"never-store-this"}))
            .send()
            .await
            .unwrap();
        assert_eq!(invalid.status(), 400);
        assert!(invalid.text().await.unwrap().contains("10 位学号"));
        assert_eq!(
            state
                .login_cache
                .get(&teaching_session::cache_key(id))
                .await
                .unwrap()
                .cookie_file,
            expected
        );
        server.abort();
    }

    #[test]
    fn csrf_changes_with_system_login_and_user() {
        let mut headers = HeaderMap::new();
        headers.insert("cookie", "assistant_token=first".parse().unwrap());
        let id = Uuid::new_v4();
        let first = csrf(&headers, id);
        assert_ne!(first, csrf(&headers, Uuid::new_v4()));
        headers.insert("cookie", "assistant_token=second".parse().unwrap());
        assert_ne!(first, csrf(&headers, id));
    }
}

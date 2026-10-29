use crate::{
    app_state::AppState, auth::session_expiry, config::SsoConfig, error::ApiError,
    store::FileUserSession,
};
use axum::{
    extract::{Query, State},
    http::{
        header::{CACHE_CONTROL, COOKIE, SET_COOKIE},
        HeaderMap, HeaderValue,
    },
    response::{IntoResponse, Redirect, Response},
    routing::get,
    Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::Utc;
use jsonwebtoken::{decode, decode_header, jwk::JwkSet, Algorithm, DecodingKey, Validation};
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub struct Transaction {
    nonce: String,
    verifier: String,
    next: String,
    created: std::time::Instant,
}
#[derive(Deserialize)]
struct StartQuery {
    next: Option<String>,
}
#[derive(Deserialize)]
struct CallbackQuery {
    state: String,
    code: Option<String>,
    error: Option<String>,
}
#[derive(Deserialize)]
struct Discovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
    userinfo_endpoint: String,
}
#[derive(Deserialize)]
struct TokenResponse {
    id_token: String,
    access_token: String,
    token_type: String,
}
#[derive(Deserialize)]
struct TokenIdentity {
    sub: String,
    nonce: String,
    iat: i64,
    aud: serde_json::Value,
    azp: Option<String>,
}
#[derive(Deserialize)]
struct Identity {
    sub: String,
    email: String,
    email_verified: bool,
    preferred_username: Option<String>,
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/auth/sso/start", get(start))
        .route("/auth/sso/callback", get(callback))
}
fn random_secret() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}
pub fn local_next(value: Option<&str>) -> String {
    value
        .filter(|v| {
            v.starts_with('/')
                && !v.starts_with("//")
                && !v.contains('\\')
                && !v.chars().any(char::is_control)
        })
        .unwrap_or("/app")
        .to_string()
}
fn validate(config: &SsoConfig) -> Result<(), ApiError> {
    if !config.enabled || config.client_id.is_empty() || config.client_secret.len() < 32 {
        return Err(ApiError::BadRequest(
            "统一认证尚未配置，请联系管理员或通过北京大学 IAAA 认证登录".into(),
        ));
    }
    for value in [&config.issuer, &config.redirect_uri] {
        let url = reqwest::Url::parse(value)
            .map_err(|_| ApiError::Internal("统一认证地址配置无效".into()))?;
        let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
        if !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || !(url.scheme() == "https"
                || (url.scheme() == "http" && (loopback || config.allow_insecure_http)))
        {
            return Err(ApiError::Internal(
                "统一认证必须使用 HTTPS，本机开发除外".into(),
            ));
        }
    }
    Ok(())
}
fn client() -> Result<reqwest::Client, ApiError> {
    Ok(reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(10))
        .build()?)
}
async fn discovery(client: &reqwest::Client, config: &SsoConfig) -> Result<Discovery, ApiError> {
    let doc: Discovery = client
        .get(format!(
            "{}/.well-known/openid-configuration",
            config.issuer.trim_end_matches('/')
        ))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    if doc.issuer.trim_end_matches('/') != config.issuer.trim_end_matches('/') {
        return Err(ApiError::Internal("统一认证 issuer 不匹配".into()));
    }
    let issuer = reqwest::Url::parse(&config.issuer).unwrap();
    for value in [
        &doc.authorization_endpoint,
        &doc.token_endpoint,
        &doc.jwks_uri,
        &doc.userinfo_endpoint,
    ] {
        let url = reqwest::Url::parse(value)
            .map_err(|_| ApiError::Internal("统一认证端点无效".into()))?;
        if url.origin() != issuer.origin() || !url.username().is_empty() || url.password().is_some()
        {
            return Err(ApiError::Internal("统一认证端点与配置的服务源不符".into()));
        }
    }
    Ok(doc)
}
fn state_cookie(value: &str, secure: bool, clear: bool) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "assistant_sso_state={value}; Path=/auth/sso; HttpOnly; SameSite=Lax; Max-Age={}{}",
        if clear { 0 } else { 600 },
        if secure { "; Secure" } else { "" }
    ))
    .expect("generated cookie")
}
async fn start(
    State(state): State<AppState>,
    Query(query): Query<StartQuery>,
) -> Result<Response, ApiError> {
    let config = &state.settings.sso;
    validate(config)?;
    let doc = discovery(&client()?, config).await?;
    let secret = random_secret();
    let nonce = random_secret();
    let verifier = random_secret();
    let mut url = reqwest::Url::parse(&doc.authorization_endpoint).unwrap();
    url.query_pairs_mut().extend_pairs([
        ("client_id", config.client_id.as_str()),
        ("redirect_uri", config.redirect_uri.as_str()),
        ("response_type", "code"),
        ("scope", "openid email profile"),
        ("state", secret.as_str()),
        ("nonce", nonce.as_str()),
        (
            "code_challenge",
            URL_SAFE_NO_PAD
                .encode(Sha256::digest(verifier.as_bytes()))
                .as_str(),
        ),
        ("code_challenge_method", "S256"),
    ]);
    let mut pending = state.sso_transactions.lock().await;
    pending.retain(|_, item| item.created.elapsed().as_secs() < 600);
    if pending.len() >= 10000 {
        return Err(ApiError::Forbidden("登录请求过多，请稍后重试".into()));
    }
    pending.insert(
        secret.clone(),
        Transaction {
            nonce,
            verifier,
            next: local_next(query.next.as_deref()),
            created: std::time::Instant::now(),
        },
    );
    Ok((
        [
            (
                SET_COOKIE,
                state_cookie(&secret, config.redirect_uri.starts_with("https:"), false),
            ),
            (CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        Redirect::to(url.as_str()),
    )
        .into_response())
}
async fn callback(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<CallbackQuery>,
) -> Result<Response, ApiError> {
    let config = &state.settings.sso;
    validate(config)?;
    let cookie = headers
        .get(COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .find_map(|piece| piece.trim().strip_prefix("assistant_sso_state="));
    if cookie != Some(query.state.as_str()) || query.state.len() < 32 {
        return Err(ApiError::BadRequest("登录状态校验失败，请重新登录".into()));
    }
    let transaction = state
        .sso_transactions
        .lock()
        .await
        .remove(&query.state)
        .filter(|item| item.created.elapsed().as_secs() < 600)
        .ok_or_else(|| ApiError::BadRequest("登录请求已过期或已使用，请重新登录".into()))?;
    if query.error.is_some() {
        return Err(ApiError::Forbidden("统一认证未完成或已取消".into()));
    }
    let code = query
        .code
        .filter(|v| !v.is_empty())
        .ok_or_else(|| ApiError::BadRequest("统一认证回调缺少授权码".into()))?;
    let http = client()?;
    let doc = discovery(&http, config).await?;
    // Client credentials and PKCE verifier travel only over the backend connection.
    let tokens: TokenResponse = http
        .post(doc.token_endpoint)
        .basic_auth(&config.client_id, Some(&config.client_secret))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", config.redirect_uri.as_str()),
            ("code_verifier", transaction.verifier.as_str()),
        ])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let header = decode_header(&tokens.id_token).map_err(|_| ApiError::Unauthorized)?;
    if header.alg != Algorithm::RS256 {
        return Err(ApiError::Unauthorized);
    }
    let jwks: JwkSet = http
        .get(doc.jwks_uri)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let key = header
        .kid
        .as_deref()
        .and_then(|kid| jwks.find(kid))
        .ok_or(ApiError::Unauthorized)?;
    let key = DecodingKey::from_jwk(key).map_err(|_| ApiError::Unauthorized)?;
    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_issuer(&[doc.issuer]);
    validation.set_audience(&[&config.client_id]);
    validation.set_required_spec_claims(&["iss", "aud", "exp", "iat", "sub", "nonce"]);
    let proof = decode::<TokenIdentity>(&tokens.id_token, &key, &validation)
        .map_err(|_| ApiError::Unauthorized)?
        .claims;
    if proof.nonce != transaction.nonce
        || proof.iat > Utc::now().timestamp() + 60
        || proof
            .azp
            .as_deref()
            .is_some_and(|azp| azp != config.client_id)
        || (proof.aud.as_array().is_some_and(|aud| aud.len() > 1)
            && proof.azp.as_deref() != Some(config.client_id.as_str()))
        || !tokens.token_type.eq_ignore_ascii_case("Bearer")
    {
        return Err(ApiError::Unauthorized);
    }
    let identity: Identity = http
        .get(doc.userinfo_endpoint)
        .bearer_auth(&tokens.access_token)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    if identity.sub != proof.sub
        || !identity.email_verified
        || identity.sub.is_empty()
        || identity.sub.len() > 255
    {
        return Err(ApiError::Unauthorized);
    }
    let email = identity.email.trim().to_lowercase();
    if !email.contains('@') || email.len() > 254 || email.chars().any(char::is_whitespace) {
        return Err(ApiError::Unauthorized);
    }
    let user = state
        .store
        .resolve_oidc_user(
            &config.issuer,
            &identity.sub,
            &email,
            identity.preferred_username.as_deref().unwrap_or(&email),
        )
        .await?;
    let session_id = Uuid::new_v4();
    let ttl = state.settings.security.session_ttl_hours;
    state
        .store
        .insert_session(FileUserSession {
            session_id,
            user_id: user.id,
            created_at: Utc::now(),
            expires_at: session_expiry(ttl),
            user_agent: headers
                .get("user-agent")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string),
            ip_address: None,
        })
        .await
        .map_err(|_| ApiError::Internal("保存登录会话失败".into()))?;
    let secure = config.redirect_uri.starts_with("https:");
    let cookie = crate::auth::browser_session_cookie(&session_id.to_string(), secure);
    tracing::info!(user_id=%user.id,"Unified authentication succeeded; application whitelist accepted");
    let mut response = Redirect::to(&transaction.next).into_response();
    response.headers_mut().append(SET_COOKIE, cookie);
    response
        .headers_mut()
        .append(SET_COOKIE, state_cookie("", secure, true));
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_external_return_targets() {
        for value in [
            "https://evil.example",
            "//evil.example",
            "/\\evil.example",
            "/\nmalicious",
        ] {
            assert_eq!(local_next(Some(value)), "/app");
        }
        assert_eq!(
            local_next(Some("/settings?tab=profile")),
            "/settings?tab=profile"
        );
    }
    #[tokio::test]
    async fn application_whitelist_and_stable_subject_are_independent_of_central_auth() {
        let dir = tempdir::TempDir::new("oidc-local-policy").unwrap();
        let store = crate::store::FileStore::new(dir.path().into())
            .await
            .unwrap();
        assert!(store
            .resolve_oidc_user(
                "http://localhost:4100",
                "subject-1",
                "student@example.com",
                "Student"
            )
            .await
            .is_err());
        std::fs::write(
            dir.path().join("whitelist.json"),
            r#"{"allowed_emails":["student@example.com","new@example.com"]}"#,
        )
        .unwrap();
        store.reload_whitelist().await.unwrap();
        let first = store
            .resolve_oidc_user(
                "http://localhost:4100",
                "subject-1",
                "student@example.com",
                "Student",
            )
            .await
            .unwrap();
        assert_eq!(first.storage_quota, 3);
        let same = store
            .resolve_oidc_user(
                "http://localhost:4100",
                "subject-1",
                "student@example.com",
                "ChangedName",
            )
            .await
            .unwrap();
        assert_eq!(first.id, same.id);
        assert!(store
            .resolve_oidc_user(
                "http://localhost:4100",
                "subject-2",
                "student@example.com",
                "Student"
            )
            .await
            .is_err());
        assert!(store
            .resolve_oidc_user(
                "http://localhost:4100",
                "subject-1",
                "new@example.com",
                "Student"
            )
            .await
            .is_err());
        std::fs::write(
            dir.path().join("whitelist.json"),
            r#"{"allowed_emails":[]}"#,
        )
        .unwrap();
        store.reload_whitelist().await.unwrap();
        assert!(store
            .resolve_oidc_user(
                "http://localhost:4100",
                "subject-1",
                "student@example.com",
                "Student"
            )
            .await
            .is_err());
    }
    #[tokio::test]
    async fn verified_central_email_links_existing_app_account_without_replacing_local_password() {
        let dir = tempdir::TempDir::new("oidc-existing").unwrap();
        let store = crate::store::FileStore::new(dir.path().into())
            .await
            .unwrap();
        let now = Utc::now();
        let id = Uuid::new_v4();
        store
            .insert_user(crate::store::FileUser {
                id,
                username: "OldName".into(),
                email: "existing@example.com".into(),
                password_hash: "original-password-hash".into(),
                is_active: true,
                created_at: now,
                updated_at: now,
                storage_quota: 21,
                processing_quota: 9,
                processing_priority: 5,
                iaaa_encrypted_password: Some("old-binding".into()),
                iaaa_public_key: None,
            })
            .await
            .unwrap();
        std::fs::write(
            dir.path().join("whitelist.json"),
            r#"{"allowed_emails":["existing@example.com"]}"#,
        )
        .unwrap();
        store.reload_whitelist().await.unwrap();
        let resolved = store
            .resolve_oidc_user(
                "http://localhost:4100",
                "new-central-user",
                "existing@example.com",
                "CentralName",
            )
            .await
            .unwrap();
        assert_eq!(resolved.id, id);
        assert_eq!(resolved.password_hash, "original-password-hash");
        assert_eq!(resolved.storage_quota, 21);
        assert_eq!(
            resolved.iaaa_encrypted_password.as_deref(),
            Some("old-binding")
        );
        let reopened = crate::store::FileStore::new(dir.path().into())
            .await
            .unwrap();
        assert_eq!(
            reopened
                .resolve_oidc_user(
                    "http://localhost:4100",
                    "new-central-user",
                    "existing@example.com",
                    "Name"
                )
                .await
                .unwrap()
                .id,
            id
        );
    }
}

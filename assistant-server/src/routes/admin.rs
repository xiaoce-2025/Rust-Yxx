use axum::{
    extract::{FromRequestParts, Path, Request, State},
    http::{header::CACHE_CONTROL, HeaderValue, Method},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::{app_state::AppState, auth::CurrentUser, error::ApiError};

pub fn routes(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/admin", get(page))
        .route("/admin/", get(page))
        .route("/api/admin/data", get(management_data))
        .route("/api/admin/users/:id", post(update_user))
        .route("/api/admin/users/:id/revoke-sessions", post(revoke_sessions))
        .route("/api/admin/whitelist", post(save_whitelist))
        .route_layer(middleware::from_fn_with_state(state, require_admin))
}

async fn require_admin(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let page = request.uri().path().starts_with("/admin");
    let (mut parts, body) = request.into_parts();
    let result = CurrentUser::from_request_parts(&mut parts, &state).await;
    let mut response = match result {
        Err(_) if page => Redirect::to("/auth?next=%2Fadmin").into_response(),
        Err(error) => error.into_response(),
        Ok(user) if !state.settings.admin.user_ids.contains(&user.id) => {
            if page {
                (axum::http::StatusCode::FORBIDDEN, Html("<!doctype html><html lang=zh-CN><meta charset=utf-8><title>无权访问</title><body style='font:16px Arial;color:#2c2020;background:#fff8f5;padding:10%'><h1>此账户没有后台访问权限</h1><p>请联系服务管理员配置账户权限。</p><a href='/'>返回首页</a></body></html>")).into_response()
            } else { ApiError::Forbidden("需要管理员权限".into()).into_response() }
        }
        Ok(user) => {
            // Browsers cannot submit this custom header through a cross-site form.
            if parts.method != Method::GET && parts.headers.get("x-admin-request").and_then(|h| h.to_str().ok()) != Some("1") {
                ApiError::Forbidden("缺少后台请求校验标识".into()).into_response()
            } else {
                parts.extensions.insert(user);
                next.run(Request::from_parts(parts, body)).await
            }
        }
    };
    response.headers_mut().insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn page() -> Html<&'static str> { Html(include_str!("../../templates/admin.html")) }

async fn management_data(State(state): State<AppState>, current: CurrentUser) -> Json<Value> {
    let counts = state.store.admin_session_counts().await;
    let mut users = state.store.admin_users().await;
    users.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(a.id.cmp(&b.id)));
    let users: Vec<Value> = users.into_iter().map(|user| json!({
        "id": user.id, "username": user.username, "email": user.email,
        "is_active": user.is_active, "created_at": user.created_at,
        "storage_quota": user.storage_quota, "processing_quota": user.processing_quota,
        "processing_priority": user.processing_priority, "sessions": counts.get(&user.id).copied().unwrap_or(0),
        "is_admin": state.settings.admin.user_ids.contains(&user.id)
    })).collect();
    // Deliberately project only operational metadata: no credentials, URLs or file paths.
    let mut jobs = Vec::new();
    for job in state.pkuvideo_jobs.read().await.values() {
        jobs.push(json!({"id":job.id,"user_id":job.user_id,"kind":"notes","status":job.status,"created_at":job.created_at,"updated_at":job.updated_at}));
    }
    for job in state.pkuvideo_fw_jobs.read().await.values() {
        jobs.push(json!({"id":job.id,"user_id":job.user_id,"kind":"replay","status":job.status,"created_at":job.created_at,"updated_at":job.updated_at}));
    }
    let entries: Vec<_> = state.video_translations.entries.lock().await.values().cloned().collect();
    for entry in entries {
        let job = entry.job.lock().await;
        jobs.push(json!({"id":job.id,"user_id":job.user_id,"kind":"translation","status":job.status,"updated_at":job.updated_at}));
    }
    jobs.sort_by(|a, b| b["updated_at"].as_str().cmp(&a["updated_at"].as_str()));
    Json(json!({"current_user_id":current.id,"username":current.username,"users":users,"jobs":jobs,
        "whitelist":state.store.admin_whitelist().await}))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UserUpdate { is_active: bool, storage_quota: i32, processing_quota: i32, processing_priority: u32 }

async fn update_user(State(state): State<AppState>, Path(id): Path<Uuid>, Json(input): Json<UserUpdate>) -> Result<Json<Value>, ApiError> {
    if !input.is_active && state.settings.admin.user_ids.contains(&id) {
        return Err(ApiError::BadRequest("不能停用管理员账户，请先在服务配置中移除其管理员权限".into()));
    }
    if input.storage_quota < 0 || input.processing_quota < 0 || !(1..=100).contains(&input.processing_priority) {
        return Err(ApiError::BadRequest("配额必须为非负整数，优先级范围为 1–100".into()));
    }
    state.store.admin_update_user(id, input.is_active, input.storage_quota, input.processing_quota, input.processing_priority).await?;
    Ok(Json(json!({"success":true})))
}

async fn revoke_sessions(State(state): State<AppState>, current: CurrentUser, Path(id): Path<Uuid>) -> Result<Json<Value>, ApiError> {
    if id == current.id { return Err(ApiError::BadRequest("请通过退出登录结束自己的会话".into())); }
    state.store.get_user(id).await.ok_or(ApiError::NotFound)?;
    let count = state.store.admin_revoke_sessions(id).await.map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(json!({"success":true,"revoked":count})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WhitelistUpdate { emails: Vec<String> }

async fn save_whitelist(State(state): State<AppState>, Json(input): Json<WhitelistUpdate>) -> Result<Json<Value>, ApiError> {
    let mut emails: Vec<String> = input.emails.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    if emails.len() > 10000 || emails.iter().any(|s| s.len() > 254 || s.chars().any(char::is_whitespace) || s.split('@').count() != 2 || s.starts_with('@') || s.ends_with('@')) {
        return Err(ApiError::BadRequest("请填写有效邮箱，每行一个，最多 10000 个".into()));
    }
    emails.sort(); emails.dedup();
    state.store.admin_save_whitelist(emails).await.map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(json!({"success":true})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Settings, store::{FileStore, FileUser, FileUserSession}};
    use chrono::{Duration, Utc};
    use std::sync::Arc;

    #[tokio::test]
    async fn admin_access_mutations_and_persistence() {
        let temp = tempdir::TempDir::new("admin").unwrap();
        let admin = Uuid::new_v4();
        let member = Uuid::new_v4();
        let settings: Settings = serde_json::from_value(json!({
            "admin":{"user_ids":[admin]},
            "server":{"address":"127.0.0.1","port":0},
            "wechat":{"api_base_url":"","oauth_app_id":"","oauth_app_secret":"","redirect_uri":"","qr_bot_type":""},
            "security":{"session_ttl_hours":1},"cors":{"allowed_origins":[],"allow_credentials":false},
            "audioapi_paraformer":{},"llm_deepseek":{}
        })).unwrap();
        let store = Arc::new(FileStore::new(temp.path().into()).await.unwrap());
        let state = AppState::new(settings, store.clone());
        let mut tokens = Vec::new();
        for id in [admin, member] {
            store.insert_user(FileUser {
                id, username: id.to_string(), email: format!("{id}@example.com"), password_hash:"SECRET_HASH".into(),
                is_active:true, created_at:Utc::now(), updated_at:Utc::now(), processing_priority:5,
                storage_quota:3, processing_quota:1, iaaa_encrypted_password:Some("SECRET_IAAA".into()), iaaa_public_key:None,
            }).await.unwrap();
            let token = Uuid::new_v4(); tokens.push(token);
            store.insert_session(FileUserSession {
                session_id:token,user_id:id,expires_at:Utc::now()+Duration::hours(1),
                created_at:Utc::now(),user_agent:None,ip_address:None,
            }).await.unwrap();
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}",listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener,crate::routes::router(state)).await.unwrap(); });
        let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
        assert_eq!(client.get(format!("{base}/admin")).send().await.unwrap().status(),303);
        assert_eq!(client.get(format!("{base}/api/admin/data")).send().await.unwrap().status(),401);
        for path in ["/admin","/api/admin/data"] {
            assert_eq!(client.get(format!("{base}{path}")).bearer_auth(tokens[1]).send().await.unwrap().status(),403);
        }
        let page = client.get(format!("{base}/admin")).bearer_auth(tokens[0]).send().await.unwrap();
        assert_eq!(page.status(),200); assert_eq!(page.headers()[CACHE_CONTROL],"no-store");
        let snapshot = client.get(format!("{base}/api/admin/data")).bearer_auth(tokens[0]).send().await.unwrap().text().await.unwrap();
        assert!(!snapshot.contains("SECRET_")); assert!(!snapshot.contains(&tokens[0].to_string()));
        assert_eq!(serde_json::from_str::<Value>(&snapshot).unwrap()["users"].as_array().unwrap().len(),2);
        let url = format!("{base}/api/admin/users/{member}");
        let update = json!({"is_active":true,"storage_quota":9,"processing_quota":8,"processing_priority":2});
        assert_eq!(client.post(&url).bearer_auth(tokens[0]).json(&update).send().await.unwrap().status(),403);
        assert_eq!(client.post(&url).bearer_auth(tokens[1]).header("x-admin-request","1").json(&update).send().await.unwrap().status(),403);
        let mut invalid = update.clone(); invalid["processing_priority"] = json!(0);
        assert_eq!(client.post(&url).bearer_auth(tokens[0]).header("x-admin-request","1").json(&invalid).send().await.unwrap().status(),400);
        assert_eq!(client.post(&url).bearer_auth(tokens[0]).header("x-admin-request","1").json(&update).send().await.unwrap().status(),200);
        invalid = update.clone(); invalid["is_active"] = json!(false);
        assert_eq!(client.post(format!("{base}/api/admin/users/{admin}")).bearer_auth(tokens[0]).header("x-admin-request","1").json(&invalid).send().await.unwrap().status(),400);
        let whitelist_url = format!("{base}/api/admin/whitelist");
        assert_eq!(client.post(&whitelist_url).bearer_auth(tokens[0]).header("x-admin-request","1").json(&json!({"emails":["invalid"]})).send().await.unwrap().status(),400);
        assert_eq!(client.post(&whitelist_url).bearer_auth(tokens[0]).header("x-admin-request","1").json(&json!({"emails":[" first@example.com ","first@example.com"]})).send().await.unwrap().status(),200);
        // Replacing an existing file must also work on Windows.
        assert_eq!(client.post(&whitelist_url).bearer_auth(tokens[0]).header("x-admin-request","1").json(&json!({"emails":["second@example.com"]})).send().await.unwrap().status(),200);
        assert_eq!(client.post(format!("{url}/revoke-sessions")).bearer_auth(tokens[0]).header("x-admin-request","1").json(&json!({})).send().await.unwrap().status(),200);
        assert!(store.get_session(tokens[1]).await.is_none());
        let restored = FileStore::new(temp.path().into()).await.unwrap();
        let user = restored.get_user(member).await.unwrap();
        assert_eq!((user.storage_quota,user.processing_quota,user.processing_priority),(9,8,2));
        assert_eq!(restored.admin_whitelist().await,vec!["second@example.com"]);
        assert!(restored.get_session(tokens[1]).await.is_none());
        // Disabling a member immediately makes its existing session unusable.
        store.insert_session(FileUserSession { session_id:tokens[1],user_id:member,expires_at:Utc::now()+Duration::hours(1),created_at:Utc::now(),user_agent:None,ip_address:None }).await.unwrap();
        assert_eq!(client.post(&url).bearer_auth(tokens[0]).header("x-admin-request","1").json(&invalid).send().await.unwrap().status(),200);
        assert_eq!(client.get(format!("{base}/api/auth/me")).bearer_auth(tokens[1]).send().await.unwrap().status(),401);
        server.abort();
    }
}

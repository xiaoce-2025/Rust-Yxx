use crate::{app_state::AppState, auth::CurrentUser};
use axum::{
    extract::{FromRequestParts, Request, State},
    http::{Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};

pub const STATUS_PATH: &str = "/api/pkuvideofullclass/batch-submit/status";
pub const START_PATH: &str = "/api/pkuvideofullclass/batch-submit";

pub async fn guard(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let path = request.uri().path();
    let method = request.method();
    // Status and authentication are local bookkeeping; the start route takes its own exclusive gate.
    if !path.starts_with("/api/")
        || (method == Method::GET && matches!(path, STATUS_PATH | "/api/auth/me"))
        || (method == Method::POST
            && matches!(
                path,
                START_PATH | "/api/pkuvideo/submit" | "/api/auth/login" | "/api/auth/iaaa"
            ))
    {
        return next.run(request).await;
    }
    let (mut parts, body) = request.into_parts();
    let current = CurrentUser::from_request_parts(&mut parts, &state).await;
    let request = Request::from_parts(parts, body);
    if let Ok(user) = current {
        let gate = state.batch_submissions.gate(user.id).await;
        // Drain requests already running before reserving a batch; no new operation can slip past reservation.
        let _operation = gate.read_owned().await;
        if state.batch_submissions.active(user.id).await {
            return (
                StatusCode::LOCKED,
                Json(serde_json::json!({
                    "code":"batch_submission_active",
                    "error":"任务正在提交中，请等待批量提交完成后再操作",
                    "submission":state.batch_submissions.snapshot(user.id).await
                })),
            )
                .into_response();
        }
        next.run(request).await
    } else {
        next.run(request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Settings,
        services::batch_submission::BatchSubmitter,
        store::{FileStore, FileUser, FileUserSession},
    };
    use chrono::Utc;
    use serde_json::{json, Value};
    use std::sync::Arc;
    use uuid::Uuid;
    struct Blocked(tokio::sync::Notify);
    #[async_trait::async_trait]
    impl BatchSubmitter for Blocked {
        async fn submit(&self, _course: &str, _video: &str) -> Result<Value, String> {
            self.0.notified().await;
            Ok(json!({"success":true,"job_id":Uuid::new_v4()}))
        }
    }
    async fn user(state: &AppState, hash: &str) -> (Uuid, Uuid, String) {
        let id = Uuid::new_v4();
        let session = Uuid::new_v4();
        let email = format!("user{}@example.com", id.simple());
        state
            .store
            .insert_user(FileUser {
                id,
                username: "test".into(),
                email: email.clone(),
                password_hash: hash.into(),
                is_active: true,
                created_at: Utc::now(),
                updated_at: Utc::now(),
                storage_quota: 5,
                processing_quota: 5,
                processing_priority: 5,
                iaaa_encrypted_password: None,
                iaaa_public_key: None,
            })
            .await
            .unwrap();
        state
            .store
            .insert_session(FileUserSession {
                session_id: session,
                user_id: id,
                expires_at: Utc::now() + chrono::Duration::hours(1),
                user_agent: None,
                ip_address: None,
                created_at: Utc::now(),
            })
            .await
            .unwrap();
        (id, session, email)
    }
    #[tokio::test]
    async fn lock_is_global_for_the_account_and_status_restores_after_navigation() {
        let temp = tempdir::TempDir::new("batch-guard").unwrap();
        let settings:Settings=serde_json::from_value(json!({
            "server":{"address":"127.0.0.1","port":0},
            "wechat":{"api_base_url":"","oauth_app_id":"","oauth_app_secret":"","redirect_uri":"","qr_bot_type":""},
            "security":{"session_ttl_hours":1},"cors":{"allowed_origins":[],"allow_credentials":false},
            "audioapi_paraformer":{},"llm_deepseek":{}
        })).unwrap();
        let state = AppState::new(
            settings,
            Arc::new(FileStore::new(temp.path().into()).await.unwrap()),
        );
        let hash = crate::auth::hash_password("test-password").unwrap();
        let (owner, session, email) = user(&state, &hash).await;
        let (_, other_session, _) = user(&state, &hash).await;
        let blocked = Arc::new(Blocked(tokio::sync::Notify::new()));
        let batch = state
            .batch_submissions
            .start(
                owner,
                "course".into(),
                vec!["lesson".into()],
                blocked.clone(),
            )
            .await
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = crate::routes::router(state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = reqwest::Client::new();
        let cookie = format!("assistant_token={session}");
        for _ in 0..2 {
            let response = client
                .get(format!("{base}{STATUS_PATH}"))
                .header("Cookie", &cookie)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()["cache-control"], "no-store");
            let restored: Value = response.json().await.unwrap();
            assert_eq!(restored["id"], batch.id.to_string());
            assert_eq!(restored["active"], true);
        }
        for path in [
            "/api/user/api-config",
            "/api/pkuvideofullclass/courses",
            "/api/pkuvideo/jobs",
        ] {
            let response = client
                .get(format!("{base}{path}"))
                .header("Cookie", &cookie)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::LOCKED);
            let body: Value = response.json().await.unwrap();
            assert_eq!(body["submission"]["id"], batch.id.to_string());
        }
        let write = client
            .post(format!("{base}/api/user/api-config"))
            .header("Cookie", &cookie)
            .json(&json!({"aliyun_model":"blocked"}))
            .send()
            .await
            .unwrap();
        assert_eq!(write.status(), StatusCode::LOCKED);
        assert!(state.store.get_config(owner).await.is_none());
        let password_api=client.post(format!("{base}/api/pkuvideo/submit")).json(&json!({
            "platform_email":email,"platform_password":"test-password","pku_auth_one":email.split('@').next().unwrap(),
            "pku_auth_two":"unused","video_url":"https://example.invalid/playlist.m3u8"
        })).send().await.unwrap();
        assert_eq!(password_api.status(), StatusCode::LOCKED);
        assert_eq!(
            client
                .get(format!("{base}/api/auth/me"))
                .header("Cookie", &cookie)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            client
                .get(format!("{base}/api/user/api-config"))
                .header("Cookie", format!("assistant_token={other_session}"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            client
                .get(format!("{base}{STATUS_PATH}"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        for path in ["/pkuvideo_sequence", "/settings", "/app"] {
            let html = client
                .get(format!("{base}{path}"))
                .header("Cookie", &cookie)
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
            assert!(html.contains("globalBatchSubmissionOverlay"));
            assert!(html.contains("任务正在提交中，请不要离开本页面"));
            assert!(
                html.find("window.BatchSubmission=").unwrap()
                    < html.find("async function api(").unwrap()
            );
        }
        blocked.0.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while state.batch_submissions.active(owner).await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            client
                .get(format!("{base}/api/user/api-config"))
                .header("Cookie", &cookie)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        server.abort();
    }
}

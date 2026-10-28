use axum::{
    extract::{FromRequestParts, Request, State},
    http::{header::CACHE_CONTROL, HeaderValue},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
    Router,
};

use crate::app_state::AppState;

const LOGIN_IMAGE_URL: &str = "/auth/login-picture.cf4a1b3511c8.webp";

const LOGIN_PREVIEW_URL: &str = "/auth/login-preview.ef807508d656.webp";

pub fn routes(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/auth", get(auth_page))
        .route("/auth/iaaa-login.css", get(|| async {
            ([(axum::http::header::CONTENT_TYPE, "text/css; charset=utf-8")],
                include_str!("../../templates/iaaa-login.css"))
        }))
        .route("/auth/login-picture.png", get(|| async {
            ([(axum::http::header::CONTENT_TYPE, "image/png")],
                include_bytes!("../../assest/login_picture.png").as_slice())
        }))
        .route(LOGIN_IMAGE_URL, get(|| async {
            ([(axum::http::header::CONTENT_TYPE, "image/webp"),
                (CACHE_CONTROL, "public, max-age=31536000, immutable")],
                include_bytes!("../../assest/login_picture.cf4a1b3511c8.webp").as_slice())
        }))
        .route(LOGIN_PREVIEW_URL, get(|| async {
            ([(axum::http::header::CONTENT_TYPE, "image/webp"),
                (CACHE_CONTROL, "public, max-age=31536000, immutable")],
                include_bytes!("../../assest/login_preview.ef807508d656.webp").as_slice())
        }))
        .route("/", get(app_page))
        .route("/app", get(app_page))
        .route("/pkuvideo", get(pkuvideo_page))
        .route("/pkuvideofullclass", get(pkuvideofullclass_page))
        .route("/pkuvideo_fw", get(pkuvideo_fw_page))
        .route("/pkuvideo_sequence", get(pkuvideo_sequence_page))
        .route("/settings", get(settings_page))
        .route_layer(middleware::from_fn_with_state(state, require_login))
}

async fn require_login(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if matches!(request.uri().path(), "/auth" | "/auth/iaaa-login.css" | "/auth/login-picture.png" | LOGIN_IMAGE_URL | LOGIN_PREVIEW_URL) {
        return next.run(request).await;
    }
    let target = request.uri().path_and_query().map(|uri| uri.as_str()).unwrap_or("/app").to_string();
    let (mut parts, body) = request.into_parts();
    let mut response = if crate::auth::CurrentUser::from_request_parts(&mut parts, &state).await.is_ok() {
        next.run(Request::from_parts(parts, body)).await
    } else {
        Redirect::to(&format!("/auth?next={}", urlencoding::encode(&target))).into_response()
    };
    response.headers_mut().insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn auth_page() -> Html<String> {
    guarded_page(include_str!("../../templates/auth.html"))
}

async fn app_page() -> Html<String> {
    guarded_page(include_str!("../../templates/app.html"))
}

async fn pkuvideo_page() -> Html<String> {
    guarded_page(include_str!("../../templates/pkuvideo.html"))
}

async fn pkuvideofullclass_page() -> Html<String> {
    guarded_page(include_str!("../../templates/pkuvideofullclass.html"))
}

async fn pkuvideo_fw_page() -> Html<String> {
    guarded_page(include_str!("../../templates/pkuvideo_fw.html"))
}

async fn pkuvideo_sequence_page() -> Html<String> {
    guarded_page(include_str!("../../templates/pkuvideo_sequence.html"))
}

async fn settings_page() -> Html<String> {
    guarded_page(include_str!("../../templates/settings.html"))
}

fn guarded_page(source: &'static str) -> Html<String> {
    Html(source.replacen("<head>", &format!("<head>{}", include_str!("../../templates/batch_submission_guard.html")), 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Settings, store::{FileStore, FileUser, FileUserSession}};
    use chrono::{Duration, Utc};
    use serde_json::json;
    use std::sync::Arc;
    use uuid::Uuid;

    #[tokio::test]
    async fn protected_html_requires_valid_session_before_rendering() {
        let temp = tempdir::TempDir::new("ui-auth").unwrap();
        let settings: Settings = serde_json::from_value(json!({
            "server":{"address":"127.0.0.1","port":0},
            "wechat":{"api_base_url":"","oauth_app_id":"","oauth_app_secret":"","redirect_uri":"","qr_bot_type":""},
            "security":{"session_ttl_hours":1},"cors":{"allowed_origins":[],"allow_credentials":false},
            "audioapi_paraformer":{},"llm_deepseek":{}
        })).unwrap();
        let state = AppState::new(settings, Arc::new(FileStore::new(temp.path().into()).await.unwrap()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = routes(state.clone()).with_state(state.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
        let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
        for path in ["/", "/app", "/settings?theme=dark", "/pkuvideo", "/pkuvideofullclass", "/pkuvideo_fw", "/pkuvideo_sequence"] {
            let response = client.get(format!("{base}{path}")).send().await.unwrap();
            assert_eq!(response.status(), 303);
            assert_eq!(response.headers()["location"], format!("/auth?next={}", urlencoding::encode(path)));
            assert_eq!(response.headers()["cache-control"], "no-store");
            assert!(response.text().await.unwrap().is_empty());
        }
        for path in ["/auth", "/auth/iaaa-login.css"] {
            assert_eq!(client.get(format!("{base}{path}")).send().await.unwrap().status(), 200);
        }
        let image = client.get(format!("{base}{LOGIN_IMAGE_URL}")).send().await.unwrap();
        assert_eq!(image.status(), 200);
        assert_eq!(image.headers()["content-type"], "image/webp");
        assert_eq!(image.headers()["cache-control"], "public, max-age=31536000, immutable");
        assert_eq!(image.bytes().await.unwrap().as_ref(), include_bytes!("../../assest/login_picture.cf4a1b3511c8.webp").as_slice());
        let preview = client.get(format!("{base}{LOGIN_PREVIEW_URL}")).send().await.unwrap();
        assert_eq!(preview.status(), 200);
        assert_eq!(preview.headers()["content-type"], "image/webp");
        assert_eq!(preview.headers()["cache-control"], "public, max-age=31536000, immutable");
        assert_eq!(preview.bytes().await.unwrap().as_ref(), include_bytes!("../../assest/login_preview.ef807508d656.webp").as_slice());
        let now = Utc::now();
        for (active, expired, expected) in [(true, false, 200), (true, true, 303), (false, false, 303)] {
            let id = Uuid::new_v4();
            state.store.insert_user(FileUser {
                id, username: id.to_string(), email: format!("{id}@example.com"), password_hash: "unused".into(),
                is_active: active, created_at: now, updated_at: now, processing_priority: 5,
                storage_quota: 3, processing_quota: 1, iaaa_encrypted_password: None, iaaa_public_key: None,
            }).await.unwrap();
            let token = Uuid::new_v4();
            state.store.insert_session(FileUserSession {
                session_id: token, user_id: id, created_at: now,
                expires_at: now + Duration::hours(if expired {-1} else {1}), user_agent: None, ip_address: None,
            }).await.unwrap();
            let response = client.get(format!("{base}/settings"))
                .header("cookie", format!("assistant_token={token}")).send().await.unwrap();
            assert_eq!(response.status().as_u16(), expected);
            assert_eq!(response.headers()["cache-control"], "no-store");
            if expected == 200 { assert!(response.text().await.unwrap().contains("<!doctype html>")); }
            else { assert!(response.text().await.unwrap().is_empty()); }
        }
        server.abort();
    }
}

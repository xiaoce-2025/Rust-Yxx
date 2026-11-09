use axum::{
    extract::{FromRequestParts, Request, State},
    http::{header::{CACHE_CONTROL, COOKIE, SET_COOKIE}, HeaderValue},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
    Router,
};

use crate::{app_state::AppState, auth::CurrentUser};

const BATCH_GUARD: &str = include_str!("../../templates/batch_submission_guard.html");

const LOGIN_IMAGE_URL: &str = "/auth/login-picture.cf4a1b3511c8.webp";

const LOGIN_PREVIEW_URL: &str = "/auth/login-preview.ef807508d656.webp";

pub fn routes(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/auth", get(auth_page))
        .route("/auth/admin", get(admin_auth_page))
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
        .route("/", get(home_page))
        .route("/home-hero-transparent.png", get(|| async {
            ([(axum::http::header::CONTENT_TYPE, "image/png")],
                include_bytes!("../../assest/home_hero_transparent.png").as_slice())
        }))
        .route("/course-replay-hero.webp", get(|| async {
            ([(axum::http::header::CONTENT_TYPE, "image/webp")],
                include_bytes!("../../assest/course-replay-hero.webp").as_slice())
        }))
        .route("/course-replay-hero-v2.webp", get(|| async {
            ([(axum::http::header::CONTENT_TYPE, "image/webp")],
                include_bytes!("../../assest/course-replay-hero-v2.webp").as_slice())
        }))
        .route("/course-replay-hero-v3.webp", get(|| async {
            ([(axum::http::header::CONTENT_TYPE, "image/webp")],
                include_bytes!("../../assest/course-replay-hero-v3.webp").as_slice())
        }))
        .route("/chat", get(app_page))
        .route("/pkuvideo", get(pkuvideo_page))
        .route("/pkuvideofullclass", get(pkuvideofullclass_page))
        .route("/pkuvideo_fullclass", get(pkuvideofullclass_page))
        .route("/pkuvideo_fw", get(pkuvideo_fw_page))
        .route("/pkuvideo_sequence", get(pkuvideo_sequence_page))
        .route("/settings", get(settings_page))
        .route("/user-center", get(user_center_page))
        .route_layer(middleware::from_fn_with_state(state, require_login))
}

async fn require_login(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if matches!(request.uri().path(), "/auth" | "/auth/admin" | "/auth/iaaa-login.css" | "/auth/login-picture.png" | LOGIN_IMAGE_URL | LOGIN_PREVIEW_URL) {
        let html = matches!(request.uri().path(), "/auth" | "/auth/admin");
        let mut response = next.run(request).await;
        if html { response.headers_mut().insert(CACHE_CONTROL, HeaderValue::from_static("no-store")); }
        return response;
    }
    let is_page = !request.uri().path().ends_with(".png")
        && !request.uri().path().ends_with(".webp");
    let target = request.uri().path_and_query().map(|uri| uri.as_str()).unwrap_or("/").to_string();
    let (mut parts, body) = request.into_parts();
    let browser_token = parts.headers.get(COOKIE).and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').find_map(|part| part.trim().strip_prefix("assistant_token=")))
        .map(str::to_owned);
    let mut response = if let Ok(user) = CurrentUser::from_request_parts(&mut parts, &state).await {
        let mut response = if is_page && state.batch_submissions.active(user.id).await {
            batch_waiting_page().into_response()
        } else {
            next.run(Request::from_parts(parts, body)).await
        };
        // Replace previously persistent cookies when an authenticated page is opened.
        if let Some(token) = browser_token {
            response.headers_mut().append(SET_COOKIE, crate::auth::browser_session_cookie(
                &token, state.settings.sso.redirect_uri.starts_with("https:"),
            ));
        }
        response
    } else {
        Redirect::to(&format!("/auth?next={}", urlencoding::encode(&target))).into_response()
    };
    response.headers_mut().insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn batch_waiting_page() -> Html<String> {
    Html(include_str!("../../templates/batch_submission_waiting.html")
        .replacen("<!-- batch-submission-guard -->", BATCH_GUARD, 1))
}

async fn auth_page(axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String,String>>) -> Html<String> {
    let next = crate::routes::sso::local_next(query.get("next").map(String::as_str));
    Html(include_str!("../../templates/auth_entry.html").replace("NEXT_ENCODED", &urlencoding::encode(&next)))
}

async fn admin_auth_page() -> Html<String> {
    guarded_page(include_str!("../../templates/auth.html"))
}

async fn app_page() -> Html<String> {
    guarded_page(include_str!("../../templates/app.html"))
}

async fn home_page() -> Html<String> {
    site_page(include_str!("../../templates/home.html"), "/")
}

async fn pkuvideo_page() -> Html<String> {
    site_page(include_str!("../../templates/pkuvideo.html"), "/pkuvideo")
}

async fn pkuvideofullclass_page() -> Html<String> {
    guarded_page(include_str!("../../templates/pkuvideofullclass.html"))
}

async fn pkuvideo_fw_page() -> Html<String> {
    replay_page(
        include_str!("../../templates/pkuvideo_fw.html"),
        "/pkuvideo_fw",
        "回放中转与下载",
        "下载课程回放并在线预览，便捷保存到本地。",
    )
}

async fn pkuvideo_sequence_page() -> Html<String> {
    replay_page(
        include_str!("../../templates/pkuvideo_sequence.html"),
        "/pkuvideo_sequence",
        "课程回放批量处理",
        "自动获取北大教学网课程列表，批量处理课程的全部回放视频，生成可下载的学习资料。",
    )
}

fn replay_page(source: &str, active_path: &str, title: &str, description: &str) -> Html<String> {
    let hero = include_str!("../../templates/replay-hero.html")
        .replace("{{replay_title}}", title)
        .replace("{{replay_description}}", description);
    let page = source
        .replacen("/* replay-hero */", include_str!("../../templates/replay-hero.css").trim_start(), 1)
        .replacen("    <!-- replay-hero -->", hero.trim_end(), 1);
    site_page(&page, active_path)
}

async fn user_center_page() -> Html<String> {
    site_page(include_str!("../../templates/user-center.html"), "/user-center")
}

async fn settings_page() -> Html<String> {
    guarded_page(include_str!("../../templates/settings.html"))
}

fn guarded_page(source: &'static str) -> Html<String> {
    site_page(source, "")
}

fn site_page(source: &str, active_path: &str) -> Html<String> {
    let css = include_str!("../../templates/site-header.css");
    let footer_css = include_str!("../../templates/site-footer.css");
    let footer = include_str!("../../templates/site-footer.html");
    let header = include_str!("../../templates/site-header.html").replacen(
        &format!("href=\"{active_path}\""),
        &format!("href=\"{active_path}\" aria-current=\"page\""),
        1,
    );
    Html(source
        .replacen("<head>", &format!("<head>{BATCH_GUARD}<style>{css}{footer_css}</style>"), 1)
        .replacen("<body>", &format!("<body>{header}"), 1)
        .replacen("<!-- site-footer -->", footer, 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Settings,
        services::batch_submission::BatchSubmitter,
        store::{FileStore, FileUser, FileUserSession},
    };
    use chrono::{Duration, Utc};
    use serde_json::json;
    use std::sync::Arc;
    use uuid::Uuid;

    struct HeldSubmission(tokio::sync::Notify);

    #[async_trait::async_trait]
    impl BatchSubmitter for HeldSubmission {
        async fn submit(&self, _: &str, _: &str) -> Result<serde_json::Value, String> {
            self.0.notified().await;
            Ok(json!({"success": true, "job_id": Uuid::new_v4()}))
        }
    }

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
        assert_eq!(client.get(format!("{base}/app")).send().await.unwrap().status(), 404);
        for path in ["/", "/chat", "/user-center", "/settings?theme=dark", "/pkuvideo", "/pkuvideofullclass", "/pkuvideo_fullclass", "/pkuvideo_fw", "/pkuvideo_sequence"] {
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
            let response = client.get(format!("{base}/user-center"))
                .header("cookie", format!("assistant_token={token}")).send().await.unwrap();
            assert_eq!(response.status().as_u16(), expected);
            assert_eq!(response.headers()["cache-control"], "no-store");
            if expected == 200 {
                let cookie = response.headers()["set-cookie"].to_str().unwrap();
                assert!(cookie.starts_with(&format!("assistant_token={token};")));
                assert!(!cookie.contains("Max-Age="));
                assert!(!cookie.contains("Expires="));
                let html = response.text().await.unwrap();
                assert!(html.contains("<h1>用户中心</h1>"));
                assert!(html.contains("href=\"/user-center\" aria-current=\"page\""));
                assert!(!html.contains("<html lang=\"zh-CN\" data-batch-waiting>"));

                let submitter = Arc::new(HeldSubmission(tokio::sync::Notify::new()));
                state.batch_submissions.start(
                    id, "course".into(), vec!["video".into()], submitter.clone(),
                ).await.unwrap();
                for path in ["/", "/user-center?tab=account", "/pkuvideo_fw"] {
                    let response = client.get(format!("{base}{path}"))
                        .header("cookie", format!("assistant_token={token}")).send().await.unwrap();
                    assert_eq!(response.status(), 200);
                    assert_eq!(response.headers()["cache-control"], "no-store");
                    let html = response.text().await.unwrap();
                    assert!(html.contains("<html lang=\"zh-CN\" data-batch-waiting>"));
                    assert!(!html.contains("<main"));
                }
                let asset = client.get(format!("{base}/course-replay-hero-v3.webp"))
                    .header("cookie", format!("assistant_token={token}")).send().await.unwrap();
                assert_eq!(asset.headers()["content-type"], "image/webp");

                submitter.0.notify_one();
                tokio::time::timeout(std::time::Duration::from_secs(2), async {
                    while state.batch_submissions.active(id).await {
                        tokio::task::yield_now().await;
                    }
                }).await.unwrap();
                let html = client.get(format!("{base}/user-center?tab=account"))
                    .header("cookie", format!("assistant_token={token}")).send().await.unwrap()
                    .text().await.unwrap();
                assert!(html.contains("<h1>用户中心</h1>"));
            }
            else { assert!(response.text().await.unwrap().is_empty()); }
        }
        server.abort();
    }
}

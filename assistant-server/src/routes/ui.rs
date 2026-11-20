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
        .route("/teaching-login", get(teaching_login_page))
        .route("/teaching-session.js", get(|| async {
            ([(axum::http::header::CONTENT_TYPE, "text/javascript; charset=utf-8"), (CACHE_CONTROL, "no-store")],
                include_str!("../../templates/teaching-session.js"))
        }))
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
        .route("/note-credits.js", get(||async {
            ([(axum::http::header::CONTENT_TYPE,"text/javascript; charset=utf-8"),(CACHE_CONTROL,"no-store")],include_str!("../../templates/note-credits.js"))
        }))
        .route("/pkuvideo", get(pkuvideo_page))
        .route("/pkuvideofullclass", get(pkuvideofullclass_page))
        .route("/pkuvideo_fullclass", get(pkuvideofullclass_page))
        .route("/pkuvideo_fw", get(pkuvideo_fw_page))
        .route("/pkuvideo_sequence", get(pkuvideo_sequence_page))
        .route("/ta", get(super::ta::page))
        .route("/settings", get(settings_page))
        .route("/user-center", get(user_center_page))
        .route("/recharge", get(super::recharge::page))
        .route("/task-history", get(task_history_page))
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

async fn teaching_login_page(user: CurrentUser, headers: axum::http::HeaderMap) -> Html<String> {
    site_page(&teaching_login_html(&super::teaching_session::csrf(&headers, user.id)), "")
}

fn teaching_login_html(csrf: &str) -> String {
    // One source of truth for the system IAAA and temporary teaching login UI.
    include_str!("../../templates/auth.html")
        .replace("<title>登录 |", "<title>一次性登录教学网 |")
        .replace("北京大学 IAAA 认证登录", "一次性登录教学网")
        .replace("使用北京大学 IAAA 账号密码认证，成功后登录对应学号邮箱账户。首次使用须通过注册白名单校验。",
            "仅用于登录教学网，不保存账号密码，也不会绑定账号。登录会话可继续使用，失效后需重新输入。")
        .replace("autocomplete=\"username\"", "autocomplete=\"off\"")
        .replace("autocomplete=\"current-password\"", "autocomplete=\"off\"")
        .replace("<form id=\"iaaaForm\"", "<form autocomplete=\"off\" id=\"iaaaForm\"")
        .replace("通过IAAA登录</button>", "一次性登录</button>")
        .replace("const teachingCsrf = \"\";", &format!("const teachingCsrf = \"{csrf}\";"))
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
    replay_page(
        include_str!("../../templates/pkuvideo.html"),
        "/pkuvideo",
        "课堂笔记生成",
        "提交课程回放链接，自动生成可下载的课堂笔记。",
    )
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

async fn task_history_page()->Html<String> {
    site_page(include_str!("../../templates/task-history.html"),"/task-history")
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

pub(crate) fn site_page(source: &str, active_path: &str) -> Html<String> {
    let css = include_str!("../../templates/site-header.css");
    let footer_css = include_str!("../../templates/site-footer.css");
    let footer = include_str!("../../templates/site-footer.html");
    let header = include_str!("../../templates/site-header.html").replacen(
        &format!("href=\"{active_path}\""),
        &format!("href=\"{active_path}\" aria-current=\"page\""),
        1,
    );
    let teaching_script = if matches!(active_path, "/" | "/ta" | "/pkuvideo_sequence" | "/pkuvideo" | "/pkuvideo_fw") {
        "<script src=\"/teaching-session.js\"></script>"
    } else { "" };
    Html(source
        .replacen("<head>", &format!("<head>{BATCH_GUARD}{teaching_script}<style>{css}{footer_css}</style>"), 1)
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

    #[test]
    fn temporary_login_reuses_iaaa_ui_without_system_login_copy_or_autofill() {
        let page = teaching_login_html("csrf-test");
        assert!(page.contains("一次性登录教学网"));
        assert!(page.contains("不保存账号密码，也不会绑定账号"));
        assert!(page.contains("const teachingCsrf = \"csrf-test\";"));
        assert!(page.contains("id=\"iaaaView\" class=\"main auth-layout\""));
        assert!(page.contains("/auth/iaaa-login.css") && page.contains(LOGIN_IMAGE_URL));
        assert!(!page.contains("首次使用须通过注册白名单校验"));
        assert!(!page.contains("autocomplete=\"current-password\""));
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
        let app = routes(state.clone())
            .merge(super::super::ta::routes())
            .merge(super::super::assignments::routes())
            .merge(super::super::pkuvideofullclass::routes())
            .with_state(state.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
        let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
        assert_eq!(client.get(format!("{base}/api/ta/workspace")).send().await.unwrap().status(), 401);
        assert_eq!(client.get(format!("{base}/app")).send().await.unwrap().status(), 404);
        for path in ["/", "/teaching-login?next=%2Fta", "/chat", "/user-center", "/settings?theme=dark", "/pkuvideo", "/pkuvideofullclass", "/pkuvideo_fullclass", "/pkuvideo_fw", "/pkuvideo_sequence", "/ta"] {
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
                iaaa_key_version: None,
                iaaa_username: None,
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
                assert!(!html.contains("隐藏已完成的作业"));
                assert!(html.contains("href=\"/user-center\" aria-current=\"page\""));
                assert!(!html.contains("<html lang=\"zh-CN\" data-batch-waiting>"));

                let cookie = format!("assistant_token={token}");
                let home = client.get(format!("{base}/")).header("cookie", &cookie)
                    .send().await.unwrap().text().await.unwrap();
                assert!(home.contains("<script src=\"/teaching-session.js\">"));
                assert!(home.contains("登录教学网账号，查看近期的ddl"));
                assert!(home.contains("id=\"assignment-refresh\""));
                let assignments_url = format!("{base}/api/assignments");
                let reminders: serde_json::Value = client.get(&assignments_url).header("cookie", &cookie)
                    .send().await.unwrap().json().await.unwrap();
                assert_eq!(reminders["login_required"], true);
                assert_eq!(reminders["refreshed_at"], serde_json::Value::Null);
                let snapshot = json!({
                    "refreshed_at": "2026-11-20T10:00:00Z",
                    "refresh_failed": false,
                    "reminders": [{
                        "title": "缓存作业", "course": "课程",
                        "deadline": "2026-11-22T23:59:00+08:00", "completed": true
                    }]
                });
                let snapshot_path = std::path::PathBuf::from("assignments")
                    .join(id.to_string()).join("snapshot.json");
                state.store.save_sidecar(&snapshot_path, &snapshot.to_string()).await.unwrap();
                let cached: serde_json::Value = client.get(assignments_url).header("cookie", &cookie)
                    .send().await.unwrap().json().await.unwrap();
                assert_eq!(cached["login_required"], true);
                assert_eq!(cached["refreshed_at"], snapshot["refreshed_at"]);
                assert_eq!(cached["reminders"][0]["title"], "缓存作业");
                assert!(cached["reminders"][0].get("completed").is_none());

                use crate::services::{course_parser::CourseInfo, course_storage, lesson_storage};
                let username = id.to_string();
                let course_id = "_replay_fixture_1";
                course_storage::save_courses(&state.store, &username, &[CourseInfo {
                    key: course_id.into(), name:"数据库课程".into(), term:"26-27学年第1学期".into(),
                }]).await.unwrap();
                let lessons = serde_json::from_value::<Vec<lesson_storage::LessonInfo>>(json!([{
                    "id":"lesson", "video_url":"", "play_url":"", "date":"2026-11-19", "time":"10:00",
                    "day_of_week":"周四", "is_processed":true, "tex_filename":"已有笔记.tex"
                }])).unwrap();
                lesson_storage::save_lessons(&state.store, &username, course_id, &lessons).await.unwrap();
                let courses_path = format!("{base}/api/pkuvideofullclass/courses");
                let courses: serde_json::Value = client.get(&courses_path).header("cookie", &cookie)
                    .send().await.unwrap().json().await.unwrap();
                assert_eq!(courses["courses"][0]["name"], "数据库课程");
                let cached: serde_json::Value = client.get(format!("{courses_path}/{course_id}/videos"))
                    .header("cookie", &cookie).send().await.unwrap().json().await.unwrap();
                assert_eq!(cached["lessons"][0]["tex_filename"], "已有笔记.tex");
                let refresh = client.post(format!("{courses_path}/refresh")).header("cookie", &cookie)
                    .send().await.unwrap();
                assert_eq!(refresh.status(), 400);
                assert!(refresh.text().await.unwrap().contains("尚未绑定教学网"));

                let once = client.get(format!("{base}/teaching-login?next=%2Fta"))
                    .header("cookie", format!("assistant_token={token}")).send().await.unwrap();
                assert_eq!(once.status(), 200);
                assert_eq!(once.headers()["cache-control"], "no-store");
                let once = once.text().await.unwrap();
                assert!(once.contains("一次性登录教学网"));
                assert!(once.contains(&format!("const teachingCsrf = \"{}\";", super::super::teaching_session::csrf(&{
                    let mut headers = axum::http::HeaderMap::new();
                    headers.insert("cookie", format!("assistant_token={token}").parse().unwrap()); headers
                }, id))));

                let ta = client.get(format!("{base}/ta"))
                    .header("cookie", format!("assistant_token={token}")).send().await.unwrap();
                assert_eq!(ta.status(),200);
                let ta = ta.text().await.unwrap();
                assert!(ta.contains("助教作业批改") && ta.contains("正在加载助教工作台"));
                assert!(ta.contains("data-loading=\"true\"") && !ta.contains("尚未绑定教学网"));
                assert!(ta.contains("href=\"/ta\" aria-current=\"page\""));
                assert!(!ta.contains("type=\"password\""));
                let data = client.get(format!("{base}/api/ta/workspace"))
                    .header("cookie", format!("assistant_token={token}")).send().await.unwrap();
                assert_eq!(data.headers()["cache-control"], "no-store");
                let data: serde_json::Value = data.json().await.unwrap();
                let fragment = data["html"].as_str().unwrap();
                assert!(fragment.contains("暂无助教课程") && fragment.contains("value=\"refresh_courses\""));
                assert!(!fragment.contains("尚未绑定教学网"));
                assert!(!fragment.contains("<html") && !fragment.contains("<script"));
                let csrf = format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(format!("ta-action-v1:{id}:{token}")));
                let failed_action = client.post(format!("{base}/ta/action"))
                    .header("cookie", format!("assistant_token={token}"))
                    .header("x-ta-partial", "1")
                    .form(&[("csrf", csrf.as_str()), ("action", "save_bulk"), ("course", "_c_1"), ("column", "_h_1")])
                    .send().await.unwrap();
                assert_eq!(failed_action.status(), 400);
                let failure: serde_json::Value = failed_action.json().await.unwrap();
                assert!(failure["html"].as_str().unwrap().contains("当前账号不是这门课程的助教"));
                assert_eq!(failure["url"], "/ta?course=_c_1&column=_h_1");
                let refresh = client.post(format!("{base}/ta/action"))
                    .header("cookie", format!("assistant_token={token}"))
                    .header("x-ta-partial", "1")
                    .form(&[("csrf", csrf.as_str()), ("action", "refresh_courses")])
                    .send().await.unwrap();
                let refresh: serde_json::Value = refresh.json().await.unwrap();
                assert!(refresh["html"].as_str().unwrap().contains("尚未绑定教学网"));

                let ta_courses_key = crate::services::ta::course_portal_cache_pattern()
                    .trim_start_matches("sidecar/").replace('*', &id.to_string());
                state.store.save_sidecar(std::path::Path::new(&ta_courses_key), &json!([
                    {"id":"_c_1","name":"已缓存的助教课一","term":""},
                    {"id":"_c_2","name":"已缓存的助教课二","term":""}
                ]).to_string()).await.unwrap();
                for query in ["", "?course=_c_2"] {
                    let data: serde_json::Value = client.get(format!("{base}/api/ta/workspace{query}"))
                        .header("cookie", &cookie).send().await.unwrap().json().await.unwrap();
                    let fragment = data["html"].as_str().unwrap();
                    assert!(fragment.contains("点击右上角「刷新作业」"));
                    assert!(fragment.contains("value=\"refresh\""));
                    assert!(!fragment.contains("尚未绑定教学网"));
                }
                let refresh: serde_json::Value = client.post(format!("{base}/ta/action"))
                    .header("cookie", &cookie).header("x-ta-partial", "1")
                    .form(&[("csrf", csrf.as_str()), ("action", "refresh"), ("course", "_c_2")])
                    .send().await.unwrap().json().await.unwrap();
                assert_eq!(refresh["url"], "/ta?course=_c_2");
                assert!(refresh["html"].as_str().unwrap().contains("尚未绑定教学网"));

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

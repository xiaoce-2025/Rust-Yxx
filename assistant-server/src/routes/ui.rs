use axum::{response::Html, routing::get, Router};

use crate::app_state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/auth", get(auth_page))
        .route("/auth/iaaa-login.css", get(|| async {
            ([(axum::http::header::CONTENT_TYPE, "text/css; charset=utf-8")],
                include_str!("../../templates/iaaa-login.css"))
        }))
        .route("/", get(app_page))
        .route("/app", get(app_page))
        .route("/pkuvideo", get(pkuvideo_page))
        .route("/pkuvideofullclass", get(pkuvideofullclass_page))
        .route("/pkuvideo_fw", get(pkuvideo_fw_page))
        .route("/pkuvideo_sequence", get(pkuvideo_sequence_page))
        .route("/settings", get(settings_page))
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

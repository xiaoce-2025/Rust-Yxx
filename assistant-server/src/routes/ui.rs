use axum::{response::Html, routing::get, Router};

use crate::app_state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/auth", get(auth_page))
        .route("/", get(app_page))
        .route("/app", get(app_page))
        .route("/pkuvideo", get(pkuvideo_page))
        .route("/settings", get(settings_page))
}

async fn auth_page() -> Html<&'static str> {
    Html(include_str!("../../templates/auth.html"))
}

async fn app_page() -> Html<&'static str> {
    Html(include_str!("../../templates/app.html"))
}

async fn pkuvideo_page() -> Html<&'static str> {
    Html(include_str!("../../templates/pkuvideo.html"))
}

async fn settings_page() -> Html<&'static str> {
    Html(include_str!("../../templates/settings.html"))
}

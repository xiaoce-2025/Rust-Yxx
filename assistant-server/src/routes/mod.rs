pub mod auth;
pub mod conversations;
pub mod health;
pub mod pkuvideo;
pub mod pkuvideofullclass;
pub mod settings;
pub mod ui;
pub mod user_api_config;
pub mod wechat;

use axum::{http::HeaderValue, routing::get, Router};
use tower_http::{
    cors::{AllowOrigin, CorsLayer},
    trace::TraceLayer,
};

use crate::app_state::AppState;

pub fn router(state: AppState) -> Router {
    let allowed_origins: Vec<HeaderValue> = state
        .settings
        .cors
        .allowed_origins
        .iter()
        .filter_map(|o| HeaderValue::from_str(o).ok())
        .collect();

    let mut cors = CorsLayer::new().allow_methods([axum::http::Method::GET, axum::http::Method::POST, axum::http::Method::DELETE]);

    if state.settings.cors.allow_credentials {
        cors = cors.allow_credentials(true);
    }

    if !allowed_origins.is_empty() {
        cors = cors.allow_origin(AllowOrigin::list(allowed_origins));
    }

    Router::new()
        .route("/health", get(health::health))
        .merge(ui::routes())
        .merge(auth::routes())
        .merge(conversations::routes())
        .merge(pkuvideo::routes())
        .merge(pkuvideofullclass::routes())
        .merge(settings::routes())
        .merge(wechat::routes())
        .merge(user_api_config::routes())
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

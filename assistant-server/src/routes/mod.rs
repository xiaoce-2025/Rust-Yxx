pub mod teaching_session;
mod iaaa_qr;
pub mod sso;
pub mod ta;
pub mod admin;
pub mod note_credits;
mod recharge;
pub mod operations;
pub mod auth;
pub mod batch_guard;
pub mod manual_lessons;
pub mod conversations;
pub mod health;
pub mod pkuvideo;
pub mod pkuvideo_fw;
pub mod pkuvideo_fw_translation;
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
        .merge(ui::routes(state.clone()))
        .merge(auth::routes())
        .merge(teaching_session::routes())
        .merge(iaaa_qr::routes())
        .merge(note_credits::routes())
        .merge(recharge::routes())
        .merge(admin::routes(state.clone()))
        .merge(operations::routes())
        .merge(sso::routes())
        .merge(conversations::routes())
        .merge(pkuvideo::routes())
        .merge(pkuvideo_fw::routes())
        .merge(pkuvideo_fw_translation::routes())
        .merge(pkuvideofullclass::routes())
        .merge(ta::routes())
        .merge(manual_lessons::routes())
        .merge(settings::routes())
        .merge(wechat::routes())
        .merge(user_api_config::routes())
        .layer(axum::middleware::from_fn_with_state(state.clone(), batch_guard::guard))
        .layer(axum::middleware::from_fn_with_state(state.clone(), operations::audit))
        .layer(cors)
        .layer(TraceLayer::new_for_http().make_span_with(|request: &axum::extract::Request| {
            let route = request.extensions().get::<axum::extract::MatchedPath>()
                .map(|path| path.as_str()).unwrap_or("<unmatched>");
            tracing::debug_span!("http.request", method = %request.method(), route)
        }))
        .with_state(state)
}

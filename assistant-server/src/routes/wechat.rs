use axum::{routing::{get, delete}, Json, Router};
use serde::Serialize;
use crate::{app_state::AppState, auth::CurrentUser, error::ApiError};

#[derive(Debug, Serialize)]
struct BindingStatusResponse {
    bound: bool,
    openid: Option<String>,
    nickname: Option<String>,
    avatar_url: Option<String>,
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/wechat/auth-url", get(get_auth_url))
        .route("/api/wechat/binding-status", get(get_binding_status))
        .route("/api/wechat/unbind", delete(unbind_wechat))
}

async fn get_auth_url(_user: CurrentUser) -> Result<Json<serde_json::Value>, ApiError> {
    Ok(Json(serde_json::json!({"auth_url": "https://example.com/auth"})))
}

async fn get_binding_status(_user: CurrentUser) -> Result<Json<BindingStatusResponse>, ApiError> {
    Ok(Json(BindingStatusResponse {
        bound: false,
        openid: None,
        nickname: None,
        avatar_url: None,
    }))
}

async fn unbind_wechat(_user: CurrentUser) -> Result<Json<serde_json::Value>, ApiError> {
    Ok(Json(serde_json::json!({"success": true})))
}

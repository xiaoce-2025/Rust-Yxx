use axum::{routing::{get, put}, Json, Router};
use serde::{Deserialize, Serialize};
use crate::{app_state::AppState, auth::CurrentUser, error::ApiError};

#[derive(Debug, Serialize)]
struct UserSettingsResponse {
    default_model: String,
    theme: String,
    openai_api_key: Option<String>,
    anthropic_api_key: Option<String>,
    settings: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct UpdateUserSettingsRequest {
    default_model: Option<String>,
    theme: Option<String>,
    openai_api_key: Option<String>,
    anthropic_api_key: Option<String>,
    settings: Option<serde_json::Value>,
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/settings/profile", get(get_user_settings))
        .route("/api/settings/profile", put(update_user_settings))
}

async fn get_user_settings(_user: CurrentUser) -> Result<Json<UserSettingsResponse>, ApiError> {
    Ok(Json(UserSettingsResponse {
        default_model: "gpt-4o-mini".to_string(),
        theme: "light".to_string(),
        openai_api_key: None,
        anthropic_api_key: None,
        settings: serde_json::json!({}),
    }))
}

async fn update_user_settings(
    _user: CurrentUser,
    Json(req): Json<UpdateUserSettingsRequest>,
) -> Result<Json<UserSettingsResponse>, ApiError> {
    Ok(Json(UserSettingsResponse {
        default_model: req.default_model.unwrap_or_else(|| "gpt-4o-mini".to_string()),
        theme: req.theme.unwrap_or_else(|| "light".to_string()),
        openai_api_key: req.openai_api_key,
        anthropic_api_key: req.anthropic_api_key,
        settings: req.settings.unwrap_or_else(|| serde_json::json!({})),
    }))
}

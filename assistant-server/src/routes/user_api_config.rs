use axum::{
    extract::State,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

use crate::{app_state::AppState, auth::CurrentUser, error::ApiError};

#[derive(Debug, Deserialize)]
struct UpdateApiConfigRequest {
    aliyun_model: Option<String>,
    aliyun_api_key: Option<String>,
    deepseek_model: Option<String>,
    deepseek_api_key: Option<String>,
}

#[derive(Debug, Serialize)]
struct ApiConfigResponse {
    aliyun_model: Option<String>,
    aliyun_api_key_set: bool, // 只返回是否设置，不返回实际值
    deepseek_model: Option<String>,
    deepseek_api_key_set: bool,
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/user/api-config", get(get_api_config))
        .route("/api/user/api-config", post(update_api_config))
}

async fn get_api_config(
    State(state): State<AppState>,
    current_user: CurrentUser,
) -> Result<Json<ApiConfigResponse>, ApiError> {
    let config = state.store.get_config(current_user.id).await;

    match config {
        Some(cfg) => Ok(Json(ApiConfigResponse {
            aliyun_model: cfg.aliyun_model,
            aliyun_api_key_set: cfg.aliyun_api_key.is_some(),
            deepseek_model: cfg.deepseek_model,
            deepseek_api_key_set: cfg.deepseek_api_key.is_some(),
        })),
        None => Ok(Json(ApiConfigResponse {
            aliyun_model: None,
            aliyun_api_key_set: false,
            deepseek_model: None,
            deepseek_api_key_set: false,
        })),
    }
}

async fn update_api_config(
    State(state): State<AppState>,
    current_user: CurrentUser,
    Json(req): Json<UpdateApiConfigRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // 获取现有配置或创建新的
    let mut config = state.store.get_config(current_user.id).await.unwrap_or_else(|| {
        crate::store::FileUserConfig {
            user_id: current_user.id,
            openai_api_key: None,
            anthropic_api_key: None,
            default_model: String::new(),
            theme: String::new(),
            settings: serde_json::Value::Object(serde_json::Map::new()),
            aliyun_model: None,
            aliyun_api_key: None,
            deepseek_model: None,
            deepseek_api_key: None,
        }
    });

    // 更新配置
    if req.aliyun_model.is_some() {
        config.aliyun_model = req.aliyun_model;
    }
    if req.aliyun_api_key.is_some() {
        config.aliyun_api_key = req.aliyun_api_key;
    }
    if req.deepseek_model.is_some() {
        config.deepseek_model = req.deepseek_model;
    }
    if req.deepseek_api_key.is_some() {
        config.deepseek_api_key = req.deepseek_api_key;
    }

    // 保存配置
    state
        .store
        .upsert_config(config)
        .await
        .map_err(|e| ApiError::Internal(format!("保存配置失败: {}", e)))?;

    tracing::info!("用户 {} 更新了API配置", current_user.id);

    Ok(Json(serde_json::json!({
        "success": true,
        "message": "配置已保存"
    })))
}

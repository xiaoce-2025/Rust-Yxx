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
    mimo_model: Option<String>,
    mimo_api_key: Option<String>,
}

#[derive(Debug, Serialize)]
struct ApiConfigResponse {
    aliyun_model: Option<String>,
    aliyun_api_key_set: bool, // 只返回是否设置，不返回实际值
    deepseek_model: Option<String>,
    deepseek_api_key_set: bool,
    mimo_model: Option<String>,
    mimo_api_key_set: bool,
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
            mimo_model: cfg.mimo_model,
            mimo_api_key_set: cfg.mimo_api_key.as_ref().is_some_and(|key| !key.trim().is_empty()),
        })),
        None => Ok(Json(ApiConfigResponse {
            aliyun_model: None,
            aliyun_api_key_set: false,
            deepseek_model: None,
            deepseek_api_key_set: false,
            mimo_model: None,
            mimo_api_key_set: false,
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
            mimo_model: None,
            mimo_api_key: None,
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
    if let Some(model) = req.mimo_model {
        config.mimo_model = nonempty(model);
    }
    if let Some(key) = req.mimo_api_key {
        config.mimo_api_key = nonempty(key);
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

fn nonempty(value: String) -> Option<String> {
    let value = value.trim();
    if value.is_empty() { None } else { Some(value.to_owned()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use uuid::Uuid;

    #[tokio::test]
    async fn mimo_configuration_saves_preserves_clears_and_never_exposes_keys() {
        let (state,temp) = crate::services::video_translation::test_state().await;
        let user = CurrentUser { id:Uuid::new_v4(),username:"test".into(),email:"test@example.com".into() };
        let request = serde_json::from_value(json!({"mimo_model":" custom-tts ","mimo_api_key":" test-user-secret "})).unwrap();
        let _ = update_api_config(State(state.clone()),user.clone(),Json(request)).await.unwrap();
        let cfg = state.store.get_config(user.id).await.unwrap();
        assert_eq!(cfg.mimo_model.as_deref(),Some("custom-tts"));
        assert_eq!(cfg.mimo_api_key.as_deref(),Some("test-user-secret"));
        let reopened = crate::store::FileStore::new(temp.path().into()).await.unwrap();
        assert_eq!(reopened.get_config(user.id).await.unwrap().mimo_model,cfg.mimo_model);

        let response = get_api_config(State(state.clone()),user.clone()).await.unwrap().0;
        let response = serde_json::to_value(response).unwrap();
        assert_eq!(response["mimo_model"],"custom-tts");
        assert_eq!(response["mimo_api_key_set"],true);
        assert!(response.get("mimo_api_key").is_none());
        assert!(!response.to_string().contains("test-user-secret"));
        let other = CurrentUser { id:Uuid::new_v4(),..user.clone() };
        assert!(!get_api_config(State(state.clone()),other).await.unwrap().0.mimo_api_key_set);

        let partial = serde_json::from_value(json!({"deepseek_model":"old-client-model"})).unwrap();
        let _ = update_api_config(State(state.clone()),user.clone(),Json(partial)).await.unwrap();
        assert_eq!(state.store.get_config(user.id).await.unwrap().mimo_api_key,cfg.mimo_api_key);
        let mut legacy = serde_json::to_value(&cfg).unwrap();
        legacy.as_object_mut().unwrap().remove("mimo_model");
        legacy.as_object_mut().unwrap().remove("mimo_api_key");
        let legacy: crate::store::FileUserConfig = serde_json::from_value(legacy).unwrap();
        assert!(legacy.mimo_model.is_none() && legacy.mimo_api_key.is_none());

        let clear = serde_json::from_value(json!({"mimo_model":"","mimo_api_key":" "})).unwrap();
        let _ = update_api_config(State(state.clone()),user.clone(),Json(clear)).await.unwrap();
        let response = get_api_config(State(state),user).await.unwrap().0;
        assert!(!response.mimo_api_key_set);
        assert!(response.mimo_model.is_none());
    }
}

use axum::{
    extract::State,
    routing::{delete, get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

use crate::{
    app_state::AppState,
    auth::CurrentUser,
    error::ApiError,
    services::pkulogin,
};

const IAAA_PUBLIC_KEY_URL: &str = "https://iaaa.pku.edu.cn/iaaa/getPublicKey.do";

#[derive(Debug, Deserialize, Serialize)]
struct IaaaPublicKeyResponse {
    success: bool,
    key: Option<String>,
}

#[derive(Debug, Serialize)]
struct BindingStatusResponse {
    is_bound: bool,
    iaaa_public_key: Option<String>,
    key_expired: bool,
    current_iaaa_key: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BindRequest {
    username: String,
    encrypted_password: String,
    iaaa_public_key: String,
}

#[derive(Debug, Serialize)]
struct Course {
    id: String,
    name: String,
    term: String,
    teacher: Option<String>,
    video_count: Option<i32>,
}

#[derive(Debug, Serialize)]
struct Video {
    id: String,
    title: String,
    duration: Option<i32>,
}

#[derive(Debug, Deserialize)]
struct BatchSubmitRequest {
    course_id: String,
    video_ids: Vec<String>,
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/pkuvideofullclass/binding-status", get(get_binding_status))
        .route("/api/pkuvideofullclass/bind", post(bind))
        .route("/api/pkuvideofullclass/unbind", delete(unbind))
        .route("/api/pkuvideofullclass/current-iaaa-key", get(get_current_iaaa_key))
        .route("/api/pkuvideofullclass/courses", get(get_courses))
        .route("/api/pkuvideofullclass/courses/:course_id/videos", get(get_course_videos))
        .route("/api/pkuvideofullclass/batch-submit", post(batch_submit))
}

async fn get_current_iaaa_key(
    _current_user: CurrentUser,
) -> Result<Json<IaaaPublicKeyResponse>, ApiError> {
    let client = reqwest::Client::new();
    let resp = client
        .get(IAAA_PUBLIC_KEY_URL)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| ApiError::Internal(format!("Failed to fetch IAAA public key: {}", e)))?;

    let data: IaaaPublicKeyResponse = resp
        .json()
        .await
        .map_err(|e| ApiError::Internal(format!("Failed to parse IAAA response: {}", e)))?;

    Ok(Json(data))
}

async fn get_binding_status(
    State(state): State<AppState>,
    current_user: CurrentUser,
) -> Result<Json<BindingStatusResponse>, ApiError> {
    let user = state
        .store
        .get_user(current_user.id)
        .await
        .ok_or(ApiError::Unauthorized)?;

    let is_bound = user.iaaa_encrypted_password.is_some() && user.iaaa_public_key.is_some();

    let mut key_expired = false;
    let mut current_iaaa_key: Option<String> = None;

    if is_bound {
        let client = reqwest::Client::new();
        match client
            .get(IAAA_PUBLIC_KEY_URL)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
        {
            Ok(resp) => {
                if let Ok(key_data) = resp.json::<IaaaPublicKeyResponse>().await {
                    current_iaaa_key = key_data.key;
                    if let Some(ref stored_key) = user.iaaa_public_key {
                        if let Some(ref current_key) = current_iaaa_key {
                            if stored_key != current_key {
                                key_expired = true;
                            }
                        }
                    }
                }
            }
            Err(_) => {
                key_expired = true;
            }
        }
    }

    Ok(Json(BindingStatusResponse {
        is_bound,
        iaaa_public_key: user.iaaa_public_key,
        key_expired,
        current_iaaa_key,
    }))
}

async fn bind(
    State(state): State<AppState>,
    current_user: CurrentUser,
    Json(req): Json<BindRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if req.username.is_empty() || req.encrypted_password.is_empty() || req.iaaa_public_key.is_empty() {
        return Err(ApiError::BadRequest("Missing required fields".to_string()));
    }

    tracing::info!("IAAA binding attempt for user {} with username {}", current_user.id, req.username);

    match pkulogin::login_iaaa_and_get_cookies(&req.username, &req.encrypted_password).await {
        Ok(login_result) => {
            tracing::info!("IAAA authentication succeeded for user {}, token obtained", req.username);
            
            // 删除临时cookie文件
            let _ = std::fs::remove_file(&login_result.cookie_file);
            
            state
                .store
                .update_iaaa_binding(current_user.id, req.encrypted_password, req.iaaa_public_key)
                .await
                .map_err(|e| {
                    tracing::error!("Failed to save IAAA binding for user {}: {}", current_user.id, e);
                    ApiError::Internal(format!("Failed to save binding: {}", e))
                })?;

            tracing::info!("IAAA binding saved successfully for user {}", current_user.id);

            Ok(Json(serde_json::json!({
                "success": true,
                "message": "绑定成功"
            })))
        }
        Err(err) => {
            tracing::error!("IAAA authentication failed for user {}: {}", req.username, err);
            Err(ApiError::BadRequest(format!("绑定失败: {}", err)))
        }
    }
}

async fn unbind(
    State(state): State<AppState>,
    current_user: CurrentUser,
) -> Result<Json<serde_json::Value>, ApiError> {
    state
        .store
        .remove_iaaa_binding(current_user.id)
        .await
        .map_err(|e| ApiError::Internal(format!("Failed to remove binding: {}", e)))?;

    Ok(Json(serde_json::json!({
        "success": true,
        "message": "解绑成功"
    })))
}

async fn get_courses(
    _current_user: CurrentUser,
) -> Result<Json<serde_json::Value>, ApiError> {
    Ok(Json(serde_json::json!({
        "success": true,
        "courses": []
    })))
}

async fn get_course_videos(
    axum::extract::Path(course_id): axum::extract::Path<String>,
    _current_user: CurrentUser,
) -> Result<Json<serde_json::Value>, ApiError> {
    Ok(Json(serde_json::json!({
        "success": true,
        "videos": []
    })))
}

async fn batch_submit(
    State(state): State<AppState>,
    current_user: CurrentUser,
    Json(req): Json<BatchSubmitRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    Ok(Json(serde_json::json!({
        "success": true,
        "job_id": "mock-job-id"
    })))
}
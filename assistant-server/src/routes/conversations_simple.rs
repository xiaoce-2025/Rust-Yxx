use axum::{routing::{get, post}, Json, Router};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use crate::{app_state::AppState, auth::CurrentUser, error::ApiError};

#[derive(Debug, Deserialize)]
struct SendMessageRequest {
    conversation_id: Option<Uuid>,
    text: String,
}

#[derive(Debug, Serialize)]
struct SendMessageResponse {
    conversation_id: Uuid,
    user_text: String,
    assistant_text: String,
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/conversations", get(list_conversations))
        .route("/api/assistant/send", post(send_message))
}

async fn list_conversations(_user: CurrentUser) -> Result<Json<Vec<serde_json::Value>>, ApiError> {
    Ok(Json(vec![]))
}

async fn send_message(
    _user: CurrentUser,
    Json(req): Json<SendMessageRequest>,
) -> Result<Json<SendMessageResponse>, ApiError> {
    if req.text.trim().is_empty() {
        return Err(ApiError::BadRequest("text 不能为空".to_string()));
    }
    Ok(Json(SendMessageResponse {
        conversation_id: Uuid::new_v4(),
        user_text: req.text,
        assistant_text: "[Demo response]你好呀，我是严小希，欢迎使用「严小希」小助手」！什么，你问我为什么这里是demo response？那当然是因为可爱的严小希想摆烂了，尽情的享受欢愉的时光吧！（我才不告诉你是之前的api服务作废了但还没有换新的呢~）".to_string(),
    }))
}

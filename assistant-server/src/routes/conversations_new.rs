use axum::{extract::{Path, State}, routing::{get, post}, Json, Router};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    app_state::AppState,
    auth::CurrentUser,
    error::ApiError,
    store::{FileConversation, FileConversationMessage},
};

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
        .route("/api/conversations/:id/messages", get(list_messages))
        .route("/api/assistant/send", post(send_message))
}

async fn list_messages(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<FileConversationMessage>>, ApiError> {
    let conv = state.store.get_conversation(id).await
        .ok_or(ApiError::NotFound)?;
    
    if conv.user_id != user.id {
        return Err(ApiError::Forbidden("无权访问".to_string()));
    }

    let messages = state.store.get_messages(id).await;
    Ok(Json(messages))
}

async fn list_conversations(
    State(state): State<AppState>,
    user: CurrentUser,
) -> Result<Json<Vec<FileConversation>>, ApiError> {
    let conversations = state.store.get_conversations(user.id).await;
    Ok(Json(conversations))
}

async fn send_message(
    State(state): State<AppState>,
    user: CurrentUser,
    Json(req): Json<SendMessageRequest>,
) -> Result<Json<SendMessageResponse>, ApiError> {
    if req.text.trim().is_empty() {
        return Err(ApiError::BadRequest("text 不能为空".to_string()));
    }

    let conversation_id = if let Some(cid) = req.conversation_id {
        let conv = state.store.get_conversation(cid).await
            .ok_or(ApiError::NotFound)?;
        if conv.user_id != user.id {
            return Err(ApiError::Forbidden("无权访问".to_string()));
        }
        cid
    } else {
        let title = req.text.chars().take(24).collect::<String>();
        let conv_id = Uuid::new_v4();
        let conv = FileConversation {
            id: conv_id,
            user_id: user.id,
            title,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        state.store.insert_conversation(conv).await
            .map_err(|e| ApiError::Internal(format!("Failed to insert conversation: {}", e)))?;
        conv_id
    };

    let assistant_text = state.plugin_router.reply(req.text.trim()).await;

    let user_msg = FileConversationMessage {
        id: Uuid::new_v4(),
        conversation_id,
        role: "user".to_string(),
        content: req.text.trim().to_string(),
        created_at: Utc::now(),
    };

    let assistant_msg = FileConversationMessage {
        id: Uuid::new_v4(),
        conversation_id,
        role: "assistant".to_string(),
        content: assistant_text.clone(),
        created_at: Utc::now(),
    };

    state.store.insert_message(user_msg).await
        .map_err(|e| ApiError::Internal(format!("Failed to insert message: {}", e)))?;

    state.store.insert_message(assistant_msg).await
        .map_err(|e| ApiError::Internal(format!("Failed to insert message: {}", e)))?;

    let mut conv = state.store.get_conversation(conversation_id).await
        .ok_or(ApiError::NotFound)?;
    conv.updated_at = Utc::now();
    state.store.update_conversation(conv).await
        .map_err(|e| ApiError::Internal(format!("Failed to update conversation: {}", e)))?;

    Ok(Json(SendMessageResponse {
        conversation_id,
        user_text: req.text,
        assistant_text,
    }))
}

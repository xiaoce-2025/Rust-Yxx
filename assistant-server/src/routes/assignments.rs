use std::{path::PathBuf, sync::Arc, time::Duration};

use axum::{extract::State, http::header::CACHE_CONTROL, routing::get, Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::{
    app_state::AppState,
    auth::CurrentUser,
    error::ApiError,
    services::{assignments, ta::Blackboard, teaching_session},
    store::FileStore,
};

#[derive(Default, Serialize, Deserialize)]
struct Snapshot {
    refreshed_at: Option<DateTime<Utc>>,
    reminders: Vec<assignments::Reminder>,
}

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/assignments", get(list))
}

fn snapshot_path(user_id: Uuid) -> PathBuf {
    PathBuf::from("assignments")
        .join(user_id.to_string())
        .join("snapshot.json")
}

pub(super) async fn refresh_lock(state: &AppState, id: Uuid) -> Arc<Mutex<()>> {
    state
        .login_cache
        .login_lock(&format!("assignment-refresh:{id}"))
        .await
}

pub(super) async fn clear_snapshot(store: &FileStore, id: Uuid) -> Result<(), ApiError> {
    store
        .delete_sidecar(&snapshot_path(id))
        .await
        .map_err(ApiError::Internal)
}

async fn list(
    State(state): State<AppState>,
    user: CurrentUser,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    let lock = refresh_lock(&state, user.id).await;
    let _guard = lock.lock().await;
    let stored = state
        .store
        .get_user(user.id)
        .await
        .ok_or(ApiError::Unauthorized)?;
    let path = snapshot_path(user.id);
    let session = teaching_session::cached(&state, &stored).await;
    let mut login_required = session.is_none();
    let mut refresh_failed = false;
    let mut fresh_snapshot = None;

    if let Some(session) = session {
        let refresh =
            async { assignments::fetch(&Blackboard::from_session(session)?, Utc::now()).await };
        let result = tokio::time::timeout(Duration::from_secs(30), refresh)
            .await
            .unwrap_or_else(|_| Err("请求教学网日历超时".into()));
        match result {
            Ok(reminders) => {
                let snapshot = Snapshot {
                    reminders,
                    refreshed_at: Some(Utc::now()),
                };
                let payload = serde_json::to_string(&snapshot)
                    .map_err(|error| ApiError::Internal(error.to_string()))?;
                state
                    .store
                    .save_sidecar(&path, &payload)
                    .await
                    .map_err(ApiError::Internal)?;
                fresh_snapshot = Some(snapshot);
            }
            Err(error) => {
                refresh_failed = true;
                login_required = teaching_session::cached(&state, &stored).await.is_none();
                tracing::warn!(user_id = %user.id, error, "刷新教学网日历失败");
            }
        }
    }

    let snapshot = match fresh_snapshot {
        Some(snapshot) => snapshot,
        None => state
            .store
            .load_sidecar(&path)
            .await
            .map_err(ApiError::Internal)?
            .unwrap_or_default(),
    };

    Ok((
        [(CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({
            "reminders": snapshot.reminders,
            "login_required": login_required,
            "refreshed_at": snapshot.refreshed_at,
            "refresh_failed": refresh_failed,
        })),
    ))
}

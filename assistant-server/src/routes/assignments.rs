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
    services::{assignments, pkulogin, ta::Blackboard, teaching_session},
    store::FileStore,
};

#[derive(Default, Serialize, Deserialize)]
struct Snapshot {
    attempted_at: Option<DateTime<Utc>>,
    refreshed_at: Option<DateTime<Utc>>,
    reminders: Vec<assignments::Reminder>,
    refresh_failed: bool,
}

#[derive(Serialize, Deserialize)]
struct Preferences {
    hide_completed: bool,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            hide_completed: true,
        }
    }
}

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/assignments", get(list)).route(
        "/api/user/assignment-preferences",
        get(preferences).put(save_preferences),
    )
}

fn path(user_id: Uuid, name: &str) -> PathBuf {
    PathBuf::from("assignments")
        .join(user_id.to_string())
        .join(name)
}

pub(super) async fn refresh_lock(state: &AppState, id: Uuid) -> Arc<Mutex<()>> {
    state
        .login_cache
        .login_lock(&format!("assignment-refresh:{id}"))
        .await
}

pub(super) async fn clear_snapshot(store: &FileStore, id: Uuid) -> Result<(), ApiError> {
    store
        .delete_sidecar(&path(id, "snapshot.json"))
        .await
        .map_err(ApiError::Internal)
}

async fn load_preferences(store: &FileStore, id: Uuid) -> Result<Preferences, ApiError> {
    Ok(store
        .load_sidecar(&path(id, "preferences.json"))
        .await
        .map_err(ApiError::Internal)?
        .unwrap_or_default())
}

async fn preferences(
    State(state): State<AppState>,
    user: CurrentUser,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    Ok((
        [(CACHE_CONTROL, "no-store")],
        Json(load_preferences(&state.store, user.id).await?),
    ))
}

async fn save_preferences(
    State(state): State<AppState>,
    user: CurrentUser,
    Json(preferences): Json<Preferences>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    save(
        &state.store,
        path(user.id, "preferences.json"),
        &preferences,
    )
    .await?;
    Ok(([(CACHE_CONTROL, "no-store")], Json(preferences)))
}

async fn save(store: &FileStore, path: PathBuf, value: &impl Serialize) -> Result<(), ApiError> {
    let payload = serde_json::to_string(value).map_err(|e| ApiError::Internal(e.to_string()))?;
    store
        .save_sidecar(&path, &payload)
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
    let bound = stored
        .iaaa_encrypted_password
        .as_deref()
        .is_some_and(|password| !password.is_empty());
    let snapshot_path = path(user.id, "snapshot.json");
    let mut snapshot: Snapshot = state
        .store
        .load_sidecar(&snapshot_path)
        .await
        .map_err(ApiError::Internal)?
        .unwrap_or_default();
    let now = Utc::now();
    let mut login_required = !bound && teaching_session::cached(&state, &stored).await.is_none();
    if !login_required && refresh_due(snapshot.attempted_at, now) {
        snapshot.attempted_at = Some(now);
        snapshot.refresh_failed = true;
        save(&state.store, snapshot_path.clone(), &snapshot).await?;
        let refresh = async {
            let session = teaching_session::login(&state, &stored).await?;
            let result = assignments::fetch(&Blackboard::from_session(session)?).await;
            if bound
                && result
                    .as_ref()
                    .is_err_and(|error| pkulogin::is_auth_error(error))
            {
                let session = teaching_session::login(&state, &stored).await?;
                return assignments::fetch(&Blackboard::from_session(session)?).await;
            }
            result
        };
        let result = tokio::time::timeout(Duration::from_secs(120), refresh).await;
        let finished_at = Utc::now();
        snapshot.attempted_at = Some(finished_at);
        match result {
            Ok(Ok(reminders)) => {
                snapshot.reminders = reminders;
                snapshot.refreshed_at = Some(finished_at);
                snapshot.refresh_failed = false;
            }
            Ok(Err(error)) => {
                login_required =
                    !bound && teaching_session::cached(&state, &stored).await.is_none();
                tracing::warn!(user_id = %user.id, error, "刷新教学网作业失败");
            }
            Err(_) => tracing::warn!(user_id = %user.id, "刷新教学网作业超时"),
        }
        save(&state.store, snapshot_path, &snapshot).await?;
    }
    let preferences = load_preferences(&state.store, user.id).await?;
    let cutoff = assignments::recent_cutoff(Utc::now());
    let reminders: Vec<_> = snapshot
        .reminders
        .into_iter()
        .filter(|entry| {
            entry.deadline >= cutoff && (!preferences.hide_completed || !entry.completed)
        })
        .collect();
    Ok((
        [(CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({
            "reminders": reminders,
            "login_required": login_required,
            "attempted_at": snapshot.attempted_at,
            "refreshed_at": snapshot.refreshed_at,
            "refresh_failed": snapshot.refresh_failed,
        })),
    ))
}

fn refresh_due(attempted_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    attempted_at.is_none_or(|last| now - last > chrono::Duration::hours(4))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_requires_strictly_more_than_four_hours() {
        let now = Utc::now();
        let four_hours_ago = now - chrono::Duration::hours(4);
        assert!(refresh_due(None, now));
        assert!(!refresh_due(Some(now), now));
        assert!(!refresh_due(Some(four_hours_ago), now));
        assert!(refresh_due(
            Some(four_hours_ago - chrono::Duration::seconds(1)),
            now
        ));
        assert!(Preferences::default().hide_completed);
    }
}

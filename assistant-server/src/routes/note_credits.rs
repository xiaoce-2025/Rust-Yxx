use crate::{app_state::AppState, auth::CurrentUser, error::ApiError};
use axum::{
    extract::{Query, State},
    http::{header::CACHE_CONTROL, HeaderValue},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/note-credits/me", get(balance))
        .route("/api/note-credits/ledger", get(ledger))
}
#[derive(Deserialize)]
pub struct LedgerQuery {
    pub limit: Option<i64>,
    pub cursor: Option<String>,
}
fn no_store(value: Value) -> Response {
    let mut response = Json(value).into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}
async fn balance(State(state): State<AppState>, user: CurrentUser) -> Result<Response, ApiError> {
    let balance = state
        .store
        .note_balance(user.id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(no_store(
        json!({"balance":balance,"unit":"hour_credit","can_start_new_job":balance>0}),
    ))
}
async fn ledger(
    State(state): State<AppState>,
    user: CurrentUser,
    Query(query): Query<LedgerQuery>,
) -> Result<Response, ApiError> {
    let value = state
        .store
        .note_ledger(user.id, query.limit.unwrap_or(30), query.cursor.as_deref())
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(no_store(value))
}

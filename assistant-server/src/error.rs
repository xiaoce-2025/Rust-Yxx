use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;

#[derive(thiserror::Error, Debug)]
pub enum ApiError {
    #[error("unauthorized")]
    Unauthorized,
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("not found")]
    NotFound,
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("internal: {0}")]
    Internal(String),
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match self {
            ApiError::Unauthorized => StatusCode::UNAUTHORIZED,
            ApiError::Forbidden(_) => StatusCode::FORBIDDEN,
            ApiError::NotFound => StatusCode::NOT_FOUND,
            ApiError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ApiError::Conflict(_) => StatusCode::CONFLICT,
            ApiError::Http(_) | ApiError::Internal(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };

        let message = match &self {
            ApiError::Http(error) => format!("上游请求失败（HTTP {:?}，超时：{}）", error.status().map(|s|s.as_u16()), error.is_timeout()),
            _ => self.to_string(),
        };
        tracing::error!(status=status.as_u16(), error=%message, "API request failed");
        let body = Json(ErrorBody { error: message });

        (status, body).into_response()
    }
}

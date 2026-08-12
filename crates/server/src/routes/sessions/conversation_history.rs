use axum::{
    Extension,
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use db::models::session::Session;
use deployment::Deployment;
use executors::{history::NativeHistoryError, logs::utils::patch::PatchType};
use serde::Serialize;
use services::services::native_history;
use ts_rs::TS;
use utils::response::ApiResponse;
use axum::{extract::State, response::Json as ResponseJson};

use crate::DeploymentImpl;

/// Structured native-history failure returned to the frontend. `code` is the
/// stable machine-readable discriminator (snake_case, append-only).
#[derive(Debug, Serialize, TS)]
pub struct NativeHistoryErrorResponse {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

pub struct NativeHistoryApiError(NativeHistoryError);

impl IntoResponse for NativeHistoryApiError {
    fn into_response(self) -> Response {
        let status = match &self.0 {
            NativeHistoryError::SessionIdMissing | NativeHistoryError::FileNotFound { .. } => {
                StatusCode::NOT_FOUND
            }
            NativeHistoryError::NotFlushed(_) => StatusCode::CONFLICT,
            NativeHistoryError::PermissionDenied { .. }
            | NativeHistoryError::FormatUnsupported(_)
            | NativeHistoryError::Corrupt { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let body = NativeHistoryErrorResponse {
            code: self.0.code().as_str().to_string(),
            message: self.0.to_string(),
            retryable: self.0.retryable(),
        };
        (status, Json(body)).into_response()
    }
}

/// Completed-session conversation: native agent history merged with
/// setup/cleanup script cards. Replaces per-process normalized-log replay.
pub async fn get_conversation_history(
    Extension(session): Extension<Session>,
    State(deployment): State<DeploymentImpl>,
) -> Result<ResponseJson<ApiResponse<Vec<PatchType>>>, NativeHistoryApiError> {
    let entries =
        native_history::get_conversation_history(&deployment.db().pool, session.id)
            .await
            .map_err(NativeHistoryApiError)?;
    Ok(ResponseJson(ApiResponse::success(entries)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The frontend keys its error UI off these strings; keep them stable.
    #[test]
    fn native_history_error_response_carries_code_and_retryable() {
        let response = NativeHistoryApiError(NativeHistoryError::NotFlushed("s-1".into()))
            .into_response();
        assert_eq!(response.status(), StatusCode::CONFLICT);

        let response = NativeHistoryApiError(NativeHistoryError::FormatUnsupported(
            "executor 'GEMINI' is not supported".into(),
        ))
        .into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}

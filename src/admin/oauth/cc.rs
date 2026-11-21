use std::sync::Arc;
use axum::extract::State;
use axum::Json;
use crate::state::AppState;

use crate::providers::oauth::commandcode::oauth::{
    self, CallbackRequest, ManualTokenRequest, OAuthStartResponse,
};

/// Start the browser hand-off OAuth flow.
/// Returns the studio auth URL with the callback pointing to the server's
/// public URL.
pub async fn start(
    State(state): State<Arc<AppState>>,
) -> Json<OAuthStartResponse> {
    let resp = oauth::start(&state.public_url).await;
    Json(resp)
}

/// POST callback from the studio page.
/// Receives { apiKey, userId, userName, state } and saves the token.
pub async fn callback(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CallbackRequest>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    match oauth::handle_callback(&state, &body).await {
        Ok(()) => Ok(Json(serde_json::json!({
            "success": true,
            "message": "Command Code OAuth connected"
        }))),
        Err(e) => Ok(Json(serde_json::json!({
            "success": false,
            "error": e
        }))),
    }
}

/// Manual token paste — user provides apiKey directly.
pub async fn manual(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ManualTokenRequest>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    match oauth::handle_manual(&state, &body).await {
        Ok(()) => Ok(Json(serde_json::json!({
            "success": true,
            "message": "Command Code token connected"
        }))),
        Err(e) => Ok(Json(serde_json::json!({
            "success": false,
            "error": e
        }))),
    }
}

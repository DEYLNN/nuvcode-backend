use std::sync::Arc;
use std::time::Instant;

use axum::Json;
use axum::response::IntoResponse;

use crate::error::GatewayError;
use crate::middleware::auth::GatewayKeyInfo;
use crate::services::usage_tracking::{canonicalize_usage, record_error, record_success};
use crate::state::AppState;
use crate::types::chat::ChatCompletionRequest;
/// Handle non-streaming chat completion.
///
/// Provider-agnostic usage tracking via [`record_success`] / [`record_error`].
/// Future engines (anthropic_compat, custom) follow the same pattern.
pub(crate) async fn handle_non_streaming(
    state: &Arc<AppState>,
    gw_key: &GatewayKeyInfo,
    provider: &(dyn crate::providers::traits::Provider + Send + Sync),
    provider_id: &str,
    model: &str,
    provider_request: &ChatCompletionRequest,
    start: Instant,
) -> Result<axum::response::Response, GatewayError> {
    let provider_id_owned = provider_id.to_string();
    let model_owned = model.to_string();
    let gateway_key_id_owned = gw_key.key_id.clone();
    let endpoint = "/v1/chat/completions".to_string();

    let result = provider.chat_completion(provider_request.clone()).await;

    match &result {
        Ok(chat_result) => {
            // Convert typed Usage to serde_json Value so canonicalize_usage works
            let usage = chat_result.response.usage.as_ref().map(|u| serde_json::json!({
                "prompt_tokens": u.prompt_tokens,
                "completion_tokens": u.completion_tokens,
            }));
            let usage = canonicalize_usage(usage.as_ref());
            // TODO: surface the underlying provider_api_key_id once engines
            // attach it to ChatCompletionResponse. For now we have no way
            // to know which specific key handled the request.
            record_success(
                &state.usage_tracker,
                start,
                &provider_id_owned,
                &model_owned,
                &gateway_key_id_owned,
                chat_result.used_key_id.as_deref(),
                &endpoint,
                usage,
                None,
            )
            .await;

            // Log failed key attempts so each key's error is visible in usage logs
            for failed in &chat_result.failed_keys {
                record_error(
                    &state.usage_tracker,
                    start,
                    &provider_id_owned,
                    &model_owned,
                    &gateway_key_id_owned,
                    Some(&failed.key_id),
                    &endpoint,
                    failed.error.http_status().unwrap_or(503) as i64,
                    &failed.error.to_string(),
                )
                .await;
            }

            Ok(Json(chat_result.response.clone()).into_response())
        }
        Err(e) => {
            record_error(
                &state.usage_tracker,
                start,
                &provider_id_owned,
                &model_owned,
                &gateway_key_id_owned,
                e.provider_api_key_id(),
                &endpoint,
                e.http_status().unwrap_or(500) as i64,
                &e.to_string(),
            )
            .await;
            // Pass through as-is — GatewayError::into_response already emits
            // the real upstream status + cleaned body (no double-wrap to 500).
            Err(e.clone())
        }
    }
}

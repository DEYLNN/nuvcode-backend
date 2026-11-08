// Chat completions module — split for maintainability.
pub mod combo;
pub mod non_streaming;
pub mod streaming;

use axum::extract::{State, Extension};
use axum::routing::post;
use axum::{Json, Router};
use std::sync::Arc;
use std::time::Instant;

use crate::error::GatewayError;
use crate::middleware::auth::GatewayKeyInfo;
use crate::services::caveman;
use crate::services::ponytail;
use crate::services::rtk;
use crate::services::tool_normalizer::normalize_tool_messages;
use crate::state::AppState;
use crate::types::chat::ChatCompletionRequest;
use crate::types::model::Model;

pub fn routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .with_state(state)
}

async fn chat_completions(
    State(state): State<Arc<AppState>>,
    Extension(gw_key): Extension<GatewayKeyInfo>,
    Json(request): Json<ChatCompletionRequest>,
) -> Result<axum::response::Response, GatewayError> {
    let start = Instant::now();
    let model = request.model.clone();

    // ── Pre-checks ──

    if crate::db::is_model_disabled(&state.db, &model).await {
        return Err(GatewayError::ModelNotFound {
            provider: "gateway".to_string(), model: model.clone(),
        });
    }

    if let Err(e) = crate::services::gateway::check_model_access(&gw_key, &model).await {
        return Err(e);
    }

    if let Err(e) = crate::services::gateway::check_token_limit(&state.db, &gw_key.key_id).await {
        return Err(e);
    }

    let (raw_prefix, model_name) = match model.split_once('/') {
        Some((pid, rest)) => (pid, rest),
        None => return Err(GatewayError::InvalidModelFormat(model.clone())),
    };
    let is_streaming = request.stream.unwrap_or(false);

    // ── Combo routing ──
    // If model starts with "combo/", resolve via combos table and dispatch
    // to the real provider. Usage is tracked under the real provider, not combo.
    if raw_prefix == "combo" {
        let combo_name = model_name.to_string();
        if is_streaming {
            return combo::handle_combo_request_stream(
                state, gw_key, request, combo_name, start,
            ).await;
        } else {
            return combo::handle_combo_request(
                state, gw_key, request, combo_name, start,
            ).await;
        }
    }

    // Blocked model check — uses the raw prefix as the provider column for now;
    // admin UI enters blocked_models with the user-facing prefix (e.g. `nx`).
    if crate::db::is_model_blocked(&state.db, raw_prefix, model_name).await {
        return Err(GatewayError::ModelNotFound {
            provider: raw_prefix.to_string(), model: model_name.to_string(),
        });
    }

    if request.messages.is_empty() {
        return Err(GatewayError::EmptyMessages);
    }

    // ── Resolve provider ──
    // `raw_prefix` may be the DB provider id (`fb`, `ocf`) or the UI model
    // prefix (e.g. `nx` for a custom provider with prefix != id). Resolve via
    // the active provider map's `model_prefix` lookup so both forms work.
    let (provider_id, provider) = {
        let pm = state.provider_manager.read().await;
        let provider_id = match pm.resolve_provider_id(raw_prefix) {
            Some(id) => id,
            None => {
                return Err(GatewayError::ProviderNotFound(raw_prefix.to_string()));
            }
        };
        let provider = match pm.get(&provider_id) {
            Some(p) => p,
            None => {
                return Err(GatewayError::ProviderNotFound(provider_id.clone()));
            }
        };
        (provider_id, provider)
    }; // read lock released here — Arc clone holds provider alive

    let mut all_models = provider.list_models().await.map_err(|_| GatewayError::Internal("Failed to list models".into()))?;
    // Merge user-added custom models so they pass the model check —
    // mirrors manager.list_all_models and the admin detail endpoint.
    for cm in crate::db::list_custom_models(&state.db, &provider_id).await {
        let id = format!("{}/{}", provider_id, cm.model_id);
        if !all_models.iter().any(|m| m.id == id) {
            all_models.push(Model {
                id,
                object: "model".to_string(),
                owned_by: provider_id.clone(),
                context_length: Some(cm.ctx as u32),
            });
        }
    }
    // Match either by full id (e.g. `nx/foo`) or by suffix (e.g. `foo`) since
    // the caller's `model` may use either the provider's UI prefix or its DB
    // row id as the leading segment.
    let model_suffix = model_name.to_string();
    if !all_models.iter().any(|m| m.id == model || m.id.ends_with(&format!("/{}", model_suffix))) {
        return Err(GatewayError::ModelNotFound {
            provider: provider_id.to_string(), model: model_name.to_string(),
        });
    }

    // ── Prepare request ──

    let mut provider_request = request.clone();
    provider_request.model = model_name.to_string();
    provider_request.stream = Some(is_streaming);
    // Inject stream_options for streaming — upstream may return usage chunks
    if is_streaming && provider_request.stream_options.is_none() {
        provider_request.stream_options = Some(serde_json::json!({"include_usage": true}));
    }
    normalize_tool_messages(&mut provider_request.messages);

    // RTK: compress tool_result content before routing
    rtk::compress(&state.db, &mut provider_request.messages).await;
    // Caveman: inject terse system prompt
    caveman::inject(&state.db, &mut provider_request.messages).await;
    // Ponytail: inject "lazy senior dev" minimalism prompt
    ponytail::inject(&state.db, &mut provider_request.messages).await;

    // ── Route to handler ──

    if is_streaming {
        streaming::handle_streaming(&state, &gw_key, provider.as_ref(), &provider_id, &model, &provider_request, start).await
    } else {
        non_streaming::handle_non_streaming(&state, &gw_key, provider.as_ref(), &provider_id, &model, &provider_request, start).await
    }
}

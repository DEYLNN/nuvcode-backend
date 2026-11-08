use async_trait::async_trait;
use std::sync::Arc;

use sqlx::PgPool;

use crate::db::models::ApiKey;
use crate::error::GatewayError;
use crate::engine::helpers::lock_key_on_error;
use crate::providers::key_manager::KeyManager;
use crate::providers::result::{ChatResult, ChatStreamResult, FailedKeyAttempt};
use crate::providers::traits::Provider;
use crate::types::chat::ChatCompletionRequest;
use crate::types::model::Model;
use crate::types::provider::ProviderMetadata;

use super::auth::CfCredential;
use super::client::CfClient;
use super::constants;

pub struct CfProvider {
    metadata: ProviderMetadata,
    keys: KeyManager,
    client: CfClient,
}

impl CfProvider {
    pub fn new_with_keys(keys: Vec<ApiKey>, db: Arc<PgPool>) -> Self {
        let metadata = ProviderMetadata {
            name: constants::PROVIDER_ID.to_string(),
            display_name: constants::PROVIDER_NAME.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            capabilities: vec!["chat".to_string(), "models".to_string(), "streaming".to_string()],
            icon_path: String::new(),
            category: constants::CATEGORY.to_string(),
            icon_name: constants::ICON_NAME.to_string(),
            color: constants::COLOR.to_string(),
            oauth_flow: None,
            model_prefix: None,
            validate_url: constants::provider_spec().validate_url.to_string(),
        };
        Self { metadata, keys: KeyManager::new_with_pool(keys, constants::PROVIDER_ID, Some((*db).clone())), client: CfClient::new() }
    }

    fn models_static(&self) -> Vec<Model> {
        constants::MODELS.iter().map(|m| Model {
            id: format!("{}/{}", constants::PROVIDER_ID, m.id.strip_prefix("@cf/").unwrap_or(m.id)),
            object: "model".to_string(),
            owned_by: constants::PROVIDER_ID.to_string(),
            context_length: Some(m.context_length),
        }).collect()
    }

    fn exhausted(&self) -> GatewayError {
        let lock_summary = self.keys.locked_keys()
            .into_iter().map(|(id, remaining, reason)| format!("{} locked {}s: {}", id, remaining, reason))
            .collect::<Vec<_>>().join("; ");
        GatewayError::NoAvailableKeys(if lock_summary.is_empty() {
            "No Cloudflare keys available".into()
        } else {
            format!("All Cloudflare keys exhausted — {}", lock_summary)
        })
    }
}

#[async_trait]
impl Provider for CfProvider {
    fn metadata(&self) -> ProviderMetadata { self.metadata.clone() }

    async fn chat_completion(&self, request: ChatCompletionRequest) -> Result<ChatResult, GatewayError> {
        let total = self.keys.total_count();
        let mut failed = Vec::new();
        for _attempt in 0..total.max(1) {
            let key = match self.keys.next() { Ok(k) => k, Err(_) => break };
            let key_id = key.id.clone();
            let cred = match CfCredential::parse(&key.key_value) {
                Ok(c) => c,
                Err(e) => { self.keys.lock_key(&key.id, 401, e.to_string()); failed.push(FailedKeyAttempt { key_id, error: GatewayError::ProviderError(e) }); continue; }
            };
            // Build body as OpenAI-compatible JSON (strip "cf/" prefix, add "@cf/" for API)
            let stripped = request.model.strip_prefix("cf/").unwrap_or(&request.model);
            let model_name = if stripped.starts_with("@cf/") { stripped.to_string() }
                else { format!("@cf/{}", stripped) };
            let body = serde_json::json!({
                "model": model_name,
                "messages": request.messages.iter().map(|m| serde_json::json!({
                    "role": m.role,
                    "content": m.content,
                })).collect::<Vec<_>>(),
                "stream": false,
                "max_tokens": request.max_tokens.unwrap_or(2048),
            });
            match self.client.send_collect(body, &cred).await {
                Ok(response) => { self.keys.mark_success(&key_id); return Ok(ChatResult { response, used_key_id: Some(key_id), failed_keys: failed }); }
                Err(e) => {
                    let c = lock_key_on_error(&self.keys, &key_id, &e);
                    if c.retryable {
                        failed.push(FailedKeyAttempt { key_id, error: e });
                        continue;
                    }
                    return Err(e);
                }
            }
        }
        Err(self.exhausted())
    }

    async fn chat_completion_stream(&self, request: ChatCompletionRequest) -> Result<ChatStreamResult, GatewayError> {
        let total = self.keys.total_count();
        let mut failed = Vec::new();
        for _attempt in 0..total.max(1) {
            let key = match self.keys.next() { Ok(k) => k, Err(_) => break };
            let key_id = key.id.clone();
            let cred = match CfCredential::parse(&key.key_value) {
                Ok(c) => c,
                Err(e) => { self.keys.lock_key(&key.id, 401, e.to_string()); failed.push(FailedKeyAttempt { key_id, error: GatewayError::ProviderError(e) }); continue; }
            };
            let stripped = request.model.strip_prefix("cf/").unwrap_or(&request.model);
            let model_name = if stripped.starts_with("@cf/") { stripped.to_string() }
                else { format!("@cf/{}", stripped) };
            let body = serde_json::json!({
                "model": model_name,
                "messages": request.messages.iter().map(|m| serde_json::json!({
                    "role": m.role,
                    "content": m.content,
                })).collect::<Vec<_>>(),
                "stream": true,
                "max_tokens": request.max_tokens.unwrap_or(2048),
            });
            match self.client.send_stream(body, &cred).await {
                Ok(stream) => { self.keys.mark_success(&key_id); return Ok(ChatStreamResult { stream, used_key_id: Some(key_id), failed_keys: failed, last_attempted_key_id: None }); }
                Err(e) => {
                    let c = lock_key_on_error(&self.keys, &key_id, &e);
                    if c.retryable {
                        failed.push(FailedKeyAttempt { key_id, error: e });
                        continue;
                    }
                    return Err(e);
                }
            }
        }
        Err(self.exhausted())
    }

    async fn list_models(&self) -> Result<Vec<Model>, GatewayError> { Ok(self.models_static()) }
    async fn health_check(&self) -> Result<bool, GatewayError> { Ok(self.keys.total_count() > 0) }
    async fn authenticate(&self) -> Result<(), GatewayError> { self.keys.next()?; Ok(()) }
    fn locked_keys(&self) -> Vec<(String, u64, String)> { self.keys.locked_keys() }
    fn total_keys(&self) -> usize { self.keys.total_count() }
    fn active_keys(&self) -> usize { self.keys.active_count() }
}

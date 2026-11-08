use async_trait::async_trait;
use std::sync::Arc;

use sqlx::PgPool;

use crate::db;
use crate::error::GatewayError;
use crate::providers::result::{ChatResult, ChatStreamResult, FailedKeyAttempt};
use crate::providers::traits::Provider;
use crate::types::chat::ChatCompletionRequest;
use crate::types::model::Model;
use crate::types::provider::ProviderMetadata;

use super::client::UnslothClient;
use super::constants;

/// Unsloth provider — mini-router: each model has its own base_url + api_key + upstream_model.
/// Config stored in `unsloth_models` table. No KeyManager — each model is self-contained.
/// Errors never lock; requests can always retry.
pub struct UnslothProvider {
    metadata: ProviderMetadata,
    db: Arc<PgPool>,
    client: UnslothClient,
}

impl UnslothProvider {
    pub fn new_with_keys(_keys: Vec<crate::db::models::ApiKey>, db: Arc<PgPool>) -> Self {
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
        Self {
            metadata,
            db,
            client: UnslothClient::new(),
        }
    }

    /// Strip `unsloth/` prefix, return the model label used in the DB.
    fn strip_prefix<'a>(&self, model: &'a str) -> &'a str {
        model.strip_prefix("uns/").unwrap_or(model)
    }

    /// Look up a model config from DB. Returns 404-style error if not found.
    async fn lookup_model(&self, model_label: &str) -> Result<db::UnslothModelRow, GatewayError> {
        db::get_unsloth_model(&self.db, model_label)
            .await
            .map_err(|e| GatewayError::ProviderError(format!("Unsloth DB: {}", e)))?
            .filter(|m| m.is_active != 0)
            .ok_or_else(|| GatewayError::ProviderError(format!("Unsloth model '{}' not found or inactive", model_label)))
    }

    fn build_body(&self, request: &ChatCompletionRequest, upstream_model: &str, stream: bool) -> serde_json::Value {
        let mut body = serde_json::json!({
            "model": upstream_model,
            "messages": request.messages.iter().filter_map(|m| serde_json::to_value(m).ok()).collect::<Vec<_>>(),
            "stream": stream,
            "max_tokens": request.max_tokens.unwrap_or(2048),
        });
        if let Some(v) = request.temperature {
            body["temperature"] = serde_json::json!(v);
        }
        if let Some(v) = request.top_p {
            body["top_p"] = serde_json::json!(v);
        }
        if let Some(ref v) = request.tools {
            body["tools"] = serde_json::to_value(v).unwrap_or_default();
        }
        if let Some(ref v) = request.tool_choice {
            body["tool_choice"] = v.clone();
        }
        if stream {
            body["stream_options"] = serde_json::json!({"include_usage": true});
        } else if let Some(ref v) = request.stream_options {
            body["stream_options"] = v.clone();
        }
        body
    }
}

#[async_trait]
impl Provider for UnslothProvider {
    fn metadata(&self) -> ProviderMetadata {
        self.metadata.clone()
    }

    async fn chat_completion(&self, request: ChatCompletionRequest) -> Result<ChatResult, GatewayError> {
        let model_label = self.strip_prefix(&request.model);
        let model_cfg = self.lookup_model(model_label).await?;
        let body = self.build_body(&request, &model_cfg.upstream_model, false);
        match self.client.send_collect(&model_cfg.base_url, body, &model_cfg.api_key).await {
            Ok(response) => Ok(ChatResult {
                response,
                used_key_id: Some(model_cfg.id.clone()),
                failed_keys: vec![],
            }),
            Err(e) => Err(e),
        }
    }

    async fn chat_completion_stream(&self, request: ChatCompletionRequest) -> Result<ChatStreamResult, GatewayError> {
        let model_label = self.strip_prefix(&request.model);
        let model_cfg = self.lookup_model(model_label).await?;
        let body = self.build_body(&request, &model_cfg.upstream_model, true);
        match self.client.send_stream(&model_cfg.base_url, body, &model_cfg.api_key).await {
            Ok(stream) => Ok(ChatStreamResult {
                stream,
                used_key_id: Some(model_cfg.id.clone()),
                failed_keys: vec![],
                last_attempted_key_id: Some(model_cfg.id),
            }),
            Err(e) => Err(e),
        }
    }

    async fn list_models(&self) -> Result<Vec<Model>, GatewayError> {
        let rows = db::list_unsloth_models(&self.db)
            .await
            .map_err(|e| GatewayError::ProviderError(format!("Unsloth DB: {}", e)))?;
        let models = rows.iter().filter(|m| m.is_active != 0).map(|m| Model {
            id: format!("{}/{}", constants::PROVIDER_ID, m.id),
            object: "model".to_string(),
            owned_by: constants::PROVIDER_ID.to_string(),
            context_length: Some(m.context_length as u32),
        }).collect();
        Ok(models)
    }

    async fn health_check(&self) -> Result<bool, GatewayError> {
        let rows = db::list_unsloth_models(&self.db)
            .await
            .map_err(|e| GatewayError::ProviderError(format!("Unsloth DB: {}", e)))?;
        Ok(rows.iter().any(|m| m.is_active != 0))
    }

    async fn authenticate(&self) -> Result<(), GatewayError> {
        Ok(())
    }

    fn locked_keys(&self) -> Vec<(String, u64, String)> {
        vec![]
    }

    fn total_keys(&self) -> usize {
        // Return active model count for display
        0 // async — can't call DB here; manager reports separately
    }

    fn active_keys(&self) -> usize {
        0
    }
}

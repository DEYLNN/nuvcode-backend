use async_trait::async_trait;
use futures::StreamExt;
use std::sync::Arc;

use sqlx::PgPool;

use crate::db::models::ApiKey;
use crate::engine::openai_compat::auth::ApiKeyAuth;
use crate::engine::openai_compat::client::Client;
use crate::engine::openai_compat::config::OpenAIConfig;
use crate::engine::openai_compat::mapper::Mapper;
use crate::error::GatewayError;
use crate::engine::helpers::lock_key_on_error;
use crate::providers::key_manager::KeyManager;
use crate::providers::result::{ChatResult, ChatStreamResult};
use crate::providers::traits::Provider;
use crate::types::chat::ChatCompletionRequest;
use crate::types::model::Model;
use crate::types::provider::ProviderMetadata;

pub struct OpenAICompatibleProvider {
    config: Arc<OpenAIConfig>,
    metadata: ProviderMetadata,
    keys: KeyManager,
    client: Client,
    mapper: Mapper,
    db: Option<PgPool>,
}

impl OpenAICompatibleProvider {
    pub fn new(config: OpenAIConfig, keys: Vec<ApiKey>, db: Option<PgPool>) -> Self {
        let config = Arc::new(config);
        let model_prefix = if config.model_prefix != config.provider_id {
            Some(config.model_prefix.clone())
        } else {
            None
        };
        let metadata = ProviderMetadata {
            name: config.provider_id.to_string(),
            display_name: config.provider_name.to_string(),
            version: format!("{}{}", config.base_url, config.chat_path),
            capabilities: vec!["chat".to_string(), "models".to_string(), "streaming".to_string()],
            icon_path: format!("/public/providers/{}.png", config.provider_id),
            category: config.category.to_string(),
            icon_name: config.icon_name.to_string(),
            color: config.color.to_string(),
            oauth_flow: None,
            validate_url: config.validate_url.clone(),
            model_prefix,
        };
        Self {
            config: config.clone(),
            metadata,
            keys: KeyManager::new_with_pool(keys, &config.provider_id, db.clone()),
            client: Client::new(config.clone()),
            mapper: Mapper::new(config),
            db,
        }
    }

    fn build_auth(&self, key: &ApiKey) -> Result<ApiKeyAuth, GatewayError> {
        if key.key_value.trim().is_empty() {
            return Err(GatewayError::ProviderError(format!(
                "Empty API key for {}",
                self.config.provider_id
            )));
        }
        Ok(ApiKeyAuth::new(key.key_value.clone()))
    }
}

#[async_trait]
impl Provider for OpenAICompatibleProvider {
    fn metadata(&self) -> ProviderMetadata {
        self.metadata.clone()
    }

    async fn chat_completion(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<ChatResult, GatewayError> {
        let provider_req = self.mapper.to_provider_request(&request);
        let total = self.keys.total_count();
        let mut excluded: Vec<String> = Vec::new();
        let mut failed: Vec<crate::providers::result::FailedKeyAttempt> = Vec::new();
        let mut last_error: Option<String> = None;
        let mut last_status: Option<u16> = None;
        let mut last_attempted_key_id: Option<String> = None;

        for _attempt in 0..total.max(1) {
            let key = match self.keys.next_excluding(None, &excluded) {
                Ok(k) => k,
                Err(_) => break,
            };
            let key_id = key.id.clone();
            last_attempted_key_id = Some(key_id.clone());
            let auth = match self.build_auth(&key) {
                Ok(a) => a,
                Err(e) => {
                    let msg = e.to_string();
                    self.keys.lock_key(&key_id, 400, msg.clone());
                    excluded.push(key_id.clone());
                    last_error = Some(msg.clone());
                    last_status = Some(400);
                    failed.push(crate::providers::result::FailedKeyAttempt { key_id: key_id.clone(), error: crate::error::GatewayError::ProviderError(msg) });
                    continue;
                }
            };
            match self.client.chat_non_streaming(&auth, &key_id, &provider_req).await {
                Ok(resp) => {
                    let gateway_resp = self.mapper.to_gateway_response(&resp);
                    self.keys.mark_success(&key_id);
                    return Ok(ChatResult {
                        response: gateway_resp,
                        used_key_id: Some(key_id),
                        failed_keys: failed,
                    });
                }
                Err(e) => {
                    let c = lock_key_on_error(&self.keys, &key_id, &e);
                    let msg = e.to_string();
                    let st = c.lock_status.unwrap_or(c.status.unwrap_or(503));
                    last_error = Some(msg.clone());
                    last_status = Some(st);
                    if c.retryable {
                        failed.push(crate::providers::result::FailedKeyAttempt { key_id: key_id.clone(), error: e });
                        excluded.push(key_id);
                        continue;
                    }
                    return Err(e);
                }
            }
        }

        // 9router pattern: return LAST attempt's actual error, not generic
        Err(GatewayError::ProviderHttpError {
            status: last_status.unwrap_or(503),
            body: last_error.unwrap_or_default(),
            provider: self.config.provider_id.clone(),
            key_id: last_attempted_key_id,
        })
    }

    async fn chat_completion_stream(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<ChatStreamResult, GatewayError> {
        let provider_req = self.mapper.to_provider_request(&request);
        let total = self.keys.total_count();
        let mut excluded: Vec<String> = Vec::new();
        let mut failed = Vec::new();
        let mut last_error: Option<String> = None;
        let mut last_status: Option<u16> = None;
        let mut last_attempted_key_id: Option<String> = None;

        for _attempt in 0..total.max(1) {
            let key = match self.keys.next_excluding(None, &excluded) {
                Ok(k) => k,
                Err(_) => break,
            };
            let key_id = key.id.clone();
            last_attempted_key_id = Some(key_id.clone());
            let auth = match self.build_auth(&key) {
                Ok(a) => a,
                Err(e) => {
                    self.keys.lock_key(&key_id, 400, e.to_string());
                    excluded.push(key_id.clone());
                    last_error = Some(e.to_string());
                    last_status = Some(400);
                    failed.push(crate::providers::result::FailedKeyAttempt { key_id: key_id.clone(), error: e });
                    continue;
                }
            };
            match self.client.chat_stream(&auth, &key_id, &provider_req).await {
                Ok(resp) => {
                    self.keys.mark_success(&key_id);
                    let _mapper = self.mapper.clone();
                    let config = self.config.clone();
                    let stream = async_stream::stream! {
                        let mut buffer = String::new();
                        let mut upstream = resp.bytes_stream();
                        let mut first_chunk_received = false;
                        loop {
                            let timeout_dur = if !first_chunk_received {
                                std::time::Duration::from_secs(config.stream_first_chunk_timeout_secs)
                            } else {
                                std::time::Duration::from_secs(config.stream_stall_timeout_secs)
                            };
                            let next = tokio::time::timeout(timeout_dur, upstream.next()).await;
                            let chunk = match next {
                                Ok(Some(Ok(b))) => b,
                                Ok(Some(Err(e))) => {
                                    yield Err(GatewayError::ProviderError(format!("Stream read error: {}", e)));
                                    break;
                                }
                                Ok(None) => break,
                                Err(_) => {
                                    yield Err(GatewayError::ProviderError("Stream timeout".into()));
                                    break;
                                }
                            };
                            buffer.push_str(&String::from_utf8_lossy(&chunk));
                            while let Some(frame_end) = buffer.find("\n\n") {
                                let frame = buffer[..frame_end].to_string();
                                buffer = buffer[frame_end + 2..].to_string();
                                for line in frame.lines() {
                                    let line = line.trim();
                                    if line.is_empty() { continue; }
                                    if line.starts_with("data: ") {
                                        let data = &line[6..];
                                        if data.trim() == "[DONE]" { break; }
                                        if let Ok(chunk) = serde_json::from_str::<crate::types::chat::ChatCompletionChunk>(data) {
                                            first_chunk_received = true;
                                            yield Ok(chunk);
                                        }
                                    }
                                }
                            }
                        }
                    };
                    return Ok(ChatStreamResult {
                        stream: stream.boxed(),
                        used_key_id: Some(key_id),
                        failed_keys: failed,
                        last_attempted_key_id,
                    });
                }
                Err(e) => {
                    let c = lock_key_on_error(&self.keys, &key_id, &e);
                    let msg = e.to_string();
                    let st = c.lock_status.unwrap_or(c.status.unwrap_or(503));
                    last_error = Some(msg.clone());
                    last_status = Some(st);
                    excluded.push(key_id.clone());
                    if c.retryable {
                        failed.push(crate::providers::result::FailedKeyAttempt { key_id: key_id.clone(), error: e });
                        continue;
                    }
                    return Err(e);
                }
            }
        }
        // 9router pattern: return LAST attempt's actual error
        Err(GatewayError::ProviderHttpError {
            status: last_status.unwrap_or(503),
            body: last_error.unwrap_or_default(),
            provider: self.config.provider_id.clone(),
            key_id: last_attempted_key_id,
        })
    }

    async fn list_models(&self) -> Result<Vec<Model>, GatewayError> {
        let mut models = self.mapper.models_static();
        // Merge user-added custom models from DB
        if let Some(pool) = &self.db {
            let custom = crate::db::list_custom_models(pool, &self.config.provider_id).await;
            let prefix = self.config.model_prefix.clone();
            for cm in custom {
                models.push(Model {
                    id: format!("{}/{}", prefix, cm.model_id),
                    object: "model".to_string(),
                    owned_by: self.config.provider_name.clone(),
                    context_length: Some(cm.ctx as u32),
                });
            }
        }
        Ok(models)
    }

    async fn health_check(&self) -> Result<bool, GatewayError> {
        Ok(self.keys.active_count() > 0)
    }

    async fn authenticate(&self) -> Result<(), GatewayError> {
        if self.keys.total_count() == 0 {
            return Err(GatewayError::ProviderError(format!(
                "No API keys configured for {}",
                self.config.provider_name
            )));
        }
        Ok(())
    }

    fn locked_keys(&self) -> Vec<(String, u64, String)> { self.keys.locked_keys() }
    fn total_keys(&self) -> usize { self.keys.total_count() }
    fn active_keys(&self) -> usize { self.keys.active_count() }
}

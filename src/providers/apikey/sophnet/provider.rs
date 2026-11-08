use async_trait::async_trait;
use std::sync::Arc;

use sqlx::PgPool;

use crate::db::models::ApiKey;
use crate::error::GatewayError;
use crate::providers::key_manager::KeyManager;
use crate::providers::result::{ChatResult, ChatStreamResult, FailedKeyAttempt};
use crate::providers::traits::Provider;
use crate::types::chat::ChatCompletionRequest;
use crate::types::model::Model;
use crate::types::provider::ProviderMetadata;

use super::auth::SopCredential;
use super::client::SopClient;
use super::constants;

/// Sophnet provider — errors do not lock or deactivate keys, EXCEPT auth/plan
/// errors (401/403). Those lock via the shared counter so 3 consecutive hits
/// auto-deactivate the key: `NO_ACTIVE_PLAN` / `MODEL_NOT_IN_TOKEN_PLAN` means
/// the account has no usable plan, so it must not be retried forever.
pub struct SopProvider {
    metadata: ProviderMetadata,
    keys: KeyManager,
    client: SopClient,
}

/// Statuses that mean the account itself is unusable, not the request.
fn is_account_error(status: Option<u16>) -> bool {
    matches!(status, Some(401) | Some(403))
}

impl SopProvider {
    pub fn new_with_keys(keys: Vec<ApiKey>, db: Arc<PgPool>) -> Self {
        let metadata = ProviderMetadata {
            name: constants::PROVIDER_ID.to_string(),
            display_name: constants::PROVIDER_NAME.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            capabilities: vec![
                "chat".to_string(),
                "models".to_string(),
                "streaming".to_string(),
            ],
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
            keys: KeyManager::new_with_pool(keys, constants::PROVIDER_ID, Some((*db).clone())),
            client: SopClient::new(),
        }
    }

    fn models_static(&self) -> Vec<Model> {
        constants::MODELS
            .iter()
            .map(|m| Model {
                id: format!("{}/{}", constants::PROVIDER_ID, m.id),
                object: "model".to_string(),
                owned_by: constants::PROVIDER_ID.to_string(),
                context_length: Some(m.context_length),
            })
            .collect()
    }

    fn build_body(&self, request: &ChatCompletionRequest, stream: bool) -> serde_json::Value {
        let model_name = request
            .model
            .strip_prefix("sop/")
            .unwrap_or(&request.model);
        let mut body = serde_json::json!({
            "model": model_name,
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
        // Reasoning effort is forced, not forwarded. Sophnet rejects every value
        // outside its five-word list with `400 invalid_params`, and a client's
        // "auto"/"none"/"minimal" would kill the whole request. Whatever the
        // caller asked for, the upstream receives SOPHNET_REASONING_EFFORT.
        if request.reasoning_effort.is_some() {
            body["reasoning_effort"] = serde_json::json!(SOPHNET_REASONING_EFFORT);
        }
        // Force include_usage on stream — required for upstream to send
        // the terminal usage chunk; without it usage tracking sees 0 tokens.
        if stream {
            body["stream_options"] = serde_json::json!({"include_usage": true});
        } else if let Some(ref v) = request.stream_options {
            body["stream_options"] = v.clone();
        }
        body
    }
}

/// The effort sent upstream whenever a caller requests reasoning at all.
/// A fixed value keeps one strict enum from failing the request: Sophnet accepts
/// only `low|medium|high|xhigh|max`, and `high` is the useful middle.
const SOPHNET_REASONING_EFFORT: &str = "high";

/// True when an upstream 400 body blames `reasoning_effort`, which is the one
/// 400 worth retrying without the field: the request itself was well-formed,
/// only that optional knob was not.
fn is_reasoning_effort_rejection(status: Option<u16>, message: &str) -> bool {
    status == Some(400) && message.contains("reasoning_effort")
}

#[async_trait]
impl Provider for SopProvider {
    fn metadata(&self) -> ProviderMetadata {
        self.metadata.clone()
    }

    async fn chat_completion(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<ChatResult, GatewayError> {
        let body = self.build_body(&request, false);
        let mut failed: Vec<FailedKeyAttempt> = Vec::new();
        let mut excluded: Vec<String> = Vec::new();
        let mut last_error: Option<String> = None;
        let mut last_status: Option<u16> = None;
        let total = self.keys.total_count();
        let mut last_attempted_key_id: Option<String> = None;
        for _ in 0..total {
            let key = match self.keys.next_excluding(None, &excluded) {
                Ok(key) => key,
                Err(_) => break,
            };
            let key_id = key.id.clone();
            last_attempted_key_id = Some(key_id.clone());
            let cred = match SopCredential::parse(&key.key_value) {
                Ok(c) => c,
                Err(e) => {
                    excluded.push(key_id.clone());
                    last_status = Some(400);
                    last_error = Some(e.clone());
                    failed.push(FailedKeyAttempt {
                        key_id,
                        error: GatewayError::ProviderError(e),
                    });
                    continue;
                }
            };
            match self.client.send_collect(body.clone(), &cred).await {
                Ok(response) => {
                    self.keys.mark_success(&key_id);
                    return Ok(ChatResult {
                        response,
                        used_key_id: Some(key_id),
                        failed_keys: failed,
                    });
                }
                Err(e) => {
                    // One retry without `reasoning_effort`: a 400 naming the field
                    // is caused by that knob alone, so the request succeeds once it
                    // is gone. Same key, same body otherwise.
                    if is_reasoning_effort_rejection(e.http_status(), &e.to_string()) {
                        let mut retry_body = body.clone();
                        if let Some(obj) = retry_body.as_object_mut() {
                            obj.remove("reasoning_effort");
                        }
                        if let Ok(response) = self.client.send_collect(retry_body, &cred).await {
                            self.keys.mark_success(&key_id);
                            return Ok(ChatResult {
                                response,
                                used_key_id: Some(key_id),
                                failed_keys: failed,
                            });
                        }
                    }
                    excluded.push(key_id.clone());
                    last_status = e.http_status();
                    if is_account_error(last_status) {
                        self.keys.lock_key(&key_id, last_status.unwrap_or(403), e.to_string());
                    }
                    last_error = Some(e.to_string());
                    failed.push(FailedKeyAttempt { key_id, error: e });
                    continue;
                }
            }
        }
        if let Some(key_id) = last_attempted_key_id {
            return Err(GatewayError::ProviderHttpError {
                status: last_status.unwrap_or(503),
                body: last_error.unwrap_or_default(),
                provider: constants::PROVIDER_ID.to_string(),
                key_id: Some(key_id),
            });
        }
        Err(GatewayError::NoAvailableKeys(
            "No Sophnet keys configured".into(),
        ))
    }

    async fn chat_completion_stream(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<ChatStreamResult, GatewayError> {
        let body = self.build_body(&request, true);
        let mut failed: Vec<FailedKeyAttempt> = Vec::new();
        let mut excluded: Vec<String> = Vec::new();
        let mut last_attempted_key_id: Option<String> = None;
        let mut last_error: Option<String> = None;
        let mut last_status: Option<u16> = None;
        let total = self.keys.total_count();
        if total == 0 {
            return Err(GatewayError::NoAvailableKeys(
                "No Sophnet keys configured".into(),
            ));
        }
        for _ in 0..total {
            let key = match self.keys.next_excluding(None, &excluded) {
                Ok(key) => key,
                Err(_) => break,
            };
            let key_id = key.id.clone();
            last_attempted_key_id = Some(key_id.clone());
            let cred = match SopCredential::parse(&key.key_value) {
                Ok(c) => c,
                Err(e) => {
                    excluded.push(key_id.clone());
                    last_status = Some(400);
                    last_error = Some(e.clone());
                    failed.push(FailedKeyAttempt {
                        key_id,
                        error: GatewayError::ProviderError(e),
                    });
                    continue;
                }
            };
            match self.client.send_stream(body.clone(), &cred).await {
                Ok(stream) => {
                    self.keys.mark_success(&key_id);
                    return Ok(ChatStreamResult {
                        stream,
                        used_key_id: Some(key_id),
                        failed_keys: failed,
                        last_attempted_key_id,
                    });
                }
                Err(e) => {
                    // Same single retry as the non-streaming path: drop only the
                    // rejected `reasoning_effort` field and re-send.
                    if is_reasoning_effort_rejection(e.http_status(), &e.to_string()) {
                        let mut retry_body = body.clone();
                        if let Some(obj) = retry_body.as_object_mut() {
                            obj.remove("reasoning_effort");
                        }
                        if let Ok(stream) = self.client.send_stream(retry_body, &cred).await {
                            self.keys.mark_success(&key_id);
                            return Ok(ChatStreamResult {
                                stream,
                                used_key_id: Some(key_id),
                                failed_keys: failed,
                                last_attempted_key_id,
                            });
                        }
                    }
                    excluded.push(key_id.clone());
                    last_status = e.http_status();
                    if is_account_error(last_status) {
                        self.keys.lock_key(&key_id, last_status.unwrap_or(403), e.to_string());
                    }
                    last_error = Some(e.to_string());
                    failed.push(FailedKeyAttempt { key_id, error: e });
                    continue;
                }
            }
        }
        Err(GatewayError::ProviderHttpError {
            status: last_status.unwrap_or(503),
            body: last_error.unwrap_or_default(),
            provider: constants::PROVIDER_ID.to_string(),
            key_id: last_attempted_key_id,
        })
    }

    async fn list_models(&self) -> Result<Vec<Model>, GatewayError> {
        Ok(self.models_static())
    }
    async fn health_check(&self) -> Result<bool, GatewayError> {
        Ok(self.keys.total_count() > 0)
    }
    async fn authenticate(&self) -> Result<(), GatewayError> {
        self.keys.next()?;
        Ok(())
    }
    fn locked_keys(&self) -> Vec<(String, u64, String)> {
        self.keys.locked_keys()
    }
    fn total_keys(&self) -> usize {
        self.keys.total_count()
    }
    fn active_keys(&self) -> usize {
        self.keys.active_count()
    }
}

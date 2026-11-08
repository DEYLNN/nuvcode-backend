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

use super::auth::OcfCredential;
use super::client::OcfClient;
use super::constants;

/// OpenCode Free provider — errors do not lock or deactivate keys.
/// Auth is always `Bearer public` regardless of DB key value.
pub struct OcfProvider {
    metadata: ProviderMetadata,
    keys: KeyManager,
    client: OcfClient,
}

impl OcfProvider {
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
            client: OcfClient::new(),
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

    fn build_body(&self, request: &ChatCompletionRequest, _stream: bool) -> serde_json::Value {
        let model_name = request.model.strip_prefix("ocf/").unwrap_or(&request.model);
        let mut body = serde_json::json!({
            "model": model_name,
            "messages": request.messages.iter().filter_map(|m| serde_json::to_value(m).ok()).collect::<Vec<_>>(),
            "stream": true, // OCF: always force stream to upstream
            "max_tokens": request.max_tokens.unwrap_or(2048),
        });
        if let Some(v) = request.temperature {
            body["temperature"] = serde_json::json!(v);
        }
        if let Some(v) = request.top_p {
            body["top_p"] = serde_json::json!(v);
        }

        let had_client_tools = if let Some(ref v) = request.tools {
            body["tools"] = serde_json::to_value(v).unwrap_or_default();
            true
        } else {
            false
        };

        // OpenCode free tier fingerprint: upstream requires tools array containing
        // the lowercase quartet (bash/glob/grep/read). Without them -> 403 FreeTierError.
        let fingerprint_tools = serde_json::json!([
            {"type":"function","function":{"name":"bash","description":"This tool is currently unavailable and must not be used.","parameters":{"type":"object","properties":{}}}},
            {"type":"function","function":{"name":"glob","description":"This tool is currently unavailable and must not be used.","parameters":{"type":"object","properties":{}}}},
            {"type":"function","function":{"name":"grep","description":"This tool is currently unavailable and must not be used.","parameters":{"type":"object","properties":{}}}},
            {"type":"function","function":{"name":"read","description":"This tool is currently unavailable and must not be used.","parameters":{"type":"object","properties":{}}}},
        ]);

        let mut existing_names = std::collections::HashSet::new();
        if let Some(arr) = body.get("tools").and_then(|t| t.as_array()) {
            for tool in arr {
                if let Some(name) = tool.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str()) {
                    existing_names.insert(name.to_lowercase());
                }
            }
        }
        let need_fingerprint = !existing_names.contains("bash")
            || !existing_names.contains("glob")
            || !existing_names.contains("grep")
            || !existing_names.contains("read");
        if need_fingerprint {
            let mut tools = body.get("tools").and_then(|t| t.as_array()).cloned().unwrap_or_default();
            if let Some(fp_arr) = fingerprint_tools.as_array() {
                for fp_tool in fp_arr {
                    if let Some(name) = fp_tool.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str()) {
                        if !existing_names.contains(name) {
                            tools.push(fp_tool.clone());
                        }
                    }
                }
            }
            body["tools"] = serde_json::json!(tools);
        }

        if let Some(ref v) = request.tool_choice {
            body["tool_choice"] = v.clone();
        } else if !had_client_tools {
            body["tool_choice"] = serde_json::json!("none");
        }

        // Force include_usage on stream — upstream always gets stream:true
        body["stream_options"] = serde_json::json!({"include_usage": true});
        body
    }
}

#[async_trait]
impl Provider for OcfProvider {
    fn metadata(&self) -> ProviderMetadata {
        self.metadata.clone()
    }

    async fn chat_completion(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<ChatResult, GatewayError> {
        // OCF: force stream to upstream, then aggregate via send_stream.
        // ponytail: send_collect (non-stream path) skipped — OCF always streams
        // upstream. Dispatcher handles aggregation for non-stream client requests.
        // To add: implement send_collect path if upstream stops supporting stream.
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
            let cred = match OcfCredential::parse(&key.key_value) {
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
                    excluded.push(key_id.clone());
                    last_status = e.http_status();
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
            "No OpenCode Free keys configured".into(),
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
                "No OpenCode Free keys configured".into(),
            ));
        }
        for _ in 0..total {
            let key = match self.keys.next_excluding(None, &excluded) {
                Ok(key) => key,
                Err(_) => break,
            };
            let key_id = key.id.clone();
            last_attempted_key_id = Some(key_id.clone());
            let cred = match OcfCredential::parse(&key.key_value) {
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
                    excluded.push(key_id.clone());
                    last_status = e.http_status();
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

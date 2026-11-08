use async_trait::async_trait;
use std::sync::Arc;

use sqlx::PgPool;
use serde_json::json;

use crate::db::models::ApiKey;
use crate::error::GatewayError;
use crate::providers::key_manager::KeyManager;
use crate::providers::result::{ChatResult, ChatStreamResult, FailedKeyAttempt};
use crate::providers::traits::Provider;
use crate::types::chat::{ChatCompletionRequest, ChatCompletionResponse};
use crate::types::model::Model;
use crate::types::provider::ProviderMetadata;

use super::auth::GbOAuthCredential;
use super::client::GbClient;
use super::constants;
use super::mapper::GbMapper;

pub struct GbProvider {
    metadata: ProviderMetadata,
    keys: KeyManager,
    client: GbClient,
    mapper: GbMapper,
    db: Option<PgPool>,
}

impl GbProvider {
    pub fn new_with_keys(keys: Vec<ApiKey>, db: Arc<PgPool>) -> Self {
        let metadata = ProviderMetadata {
            name: constants::PROVIDER_ID.to_string(),
            display_name: constants::PROVIDER_NAME.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            capabilities: vec![
                "chat".to_string(),
                "models".to_string(),
                "streaming".to_string(),
                "oauth".to_string(),
            ],
            icon_path: format!("/public/providers/{}.png", constants::PROVIDER_ID),
            category: constants::CATEGORY.to_string(),
            icon_name: constants::ICON_NAME.to_string(),
            color: constants::COLOR.to_string(),
            oauth_flow: Some("device_code".to_string()),
            validate_url: constants::VALIDATE_URL.to_string(),
            model_prefix: None,
        };
        let db_pool = Some((*db).clone());
        Self {
            metadata,
            keys: KeyManager::new_with_pool(keys, constants::PROVIDER_ID, Some((*db).clone())),
            client: GbClient::new(),
            mapper: GbMapper,
            db: db_pool,
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

    async fn refresh_if_needed(&self, key: &ApiKey, cred: GbOAuthCredential) -> Result<GbOAuthCredential, GatewayError> {
        if !cred.needs_refresh(chrono::Utc::now().timestamp(), 300) {
            return Ok(cred);
        }
        let refresh_token = cred.refresh_token.clone().ok_or_else(|| GatewayError::ProviderError("GrokBuild: refresh_token missing".into()))?;
        let data = super::oauth::refresh(&refresh_token).await.map_err(GatewayError::ProviderError)?;
        let access_token = data["access_token"].as_str().ok_or_else(|| GatewayError::ProviderError("GrokBuild refresh: access_token missing".into()))?;
        let expires_at = data["expires_in"].as_i64().map(|s| chrono::Utc::now().timestamp() + s).or(cred.expires_at);
        let new_refresh = data["refresh_token"].as_str().unwrap_or(&refresh_token);
        let key_value = serde_json::to_string(&json!({"access_token": access_token, "refresh_token": new_refresh, "expires_at": expires_at, "email": cred.email}))
            .map_err(|e| GatewayError::ProviderError(format!("GrokBuild refresh serialize: {e}")))?;
        sqlx::query("UPDATE api_keys SET key_value = $1, updated_at = $2 WHERE id = $3")
            .bind(&key_value).bind(chrono::Utc::now().to_rfc3339()).bind(&key.id)
            .execute(self.db.as_ref().ok_or_else(|| GatewayError::ProviderError("GrokBuild: DB unavailable".into()))?)
            .await.map_err(|e| GatewayError::ProviderError(format!("GrokBuild refresh DB: {e}")))?;
        GbOAuthCredential::parse(&key_value).map_err(GatewayError::ProviderError)
    }
}

#[async_trait]
impl Provider for GbProvider {
    fn metadata(&self) -> ProviderMetadata {
        self.metadata.clone()
    }

    fn total_keys(&self) -> usize {
        self.keys.total_count()
    }
    fn active_keys(&self) -> usize {
        self.keys.active_count()
    }
    fn locked_keys(&self) -> Vec<(String, u64, String)> {
        self.keys.locked_keys()
    }

    async fn health_check(&self) -> Result<bool, GatewayError> {
        Ok(self.keys.active_count() > 0)
    }

    async fn authenticate(&self) -> Result<(), GatewayError> {
        if self.keys.active_count() == 0 {
            return Err(GatewayError::ProviderError(
                "GrokBuild: no active keys".to_string(),
            ));
        }
        Ok(())
    }

    async fn chat_completion(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<ChatResult, GatewayError> {
        let total = self.keys.total_count();
        let mut excluded: Vec<String> = Vec::new();
        let mut failed: Vec<FailedKeyAttempt> = Vec::new();
        let mut last_error: Option<String> = None;
        let mut last_status: Option<u16> = None;

        for _attempt in 0..total.max(1) {
            let key = match self.keys.next_excluding(None, &excluded) {
                Ok(k) => k,
                Err(_) => break,
            };
            let key_id = key.id.clone();
            let cred = match GbOAuthCredential::parse(&key.key_value) {
                Ok(c) => c,
                Err(e) => {
                    let msg = e;
                    self.keys.lock_key(&key_id, 400, msg.clone());
                    excluded.push(key_id.clone());
                    last_error = Some(msg.clone());
                    last_status = Some(400);
                    failed.push(FailedKeyAttempt {
                        key_id: key_id.clone(),
                        error: GatewayError::ProviderError(msg),
                    });
                    continue;
                }
            };
            let cred = match self.refresh_if_needed(&key, cred).await {
                Ok(c) => c,
                Err(e) => { excluded.push(key_id.clone()); last_error = Some(e.to_string()); last_status = Some(502); failed.push(FailedKeyAttempt { key_id: key_id.clone(), error: e }); continue; }
            };
            let body = self.mapper.to_responses_request(request.clone());
            match self.client.send_collect(body, &cred).await {
                Ok(upstream) => {
                    let response: ChatCompletionResponse =
                        self.client.collect_to_chat_response(&upstream);
                    self.keys.mark_success(&key_id);
                    return Ok(ChatResult {
                        response,
                        used_key_id: Some(key_id),
                        failed_keys: failed,
                    });
                }
                Err(e) => {
                    let msg = e.to_string();
                    let st = e.http_status().unwrap_or(502);
                    self.keys.lock_key(&key_id, st, msg.clone());
                    last_error = Some(msg.clone());
                    last_status = Some(st);
                    failed.push(FailedKeyAttempt {
                        key_id: key_id.clone(),
                        error: e,
                    });
                    excluded.push(key_id);
                    continue;
                }
            }
        }
        Err(GatewayError::ProviderHttpError {
            status: last_status.unwrap_or(503),
            body: last_error.unwrap_or_default(),
            provider: self.metadata.name.clone(),
            key_id: None,
        })
    }

    async fn chat_completion_stream(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<ChatStreamResult, GatewayError> {
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
            let cred = match GbOAuthCredential::parse(&key.key_value) {
                Ok(c) => c,
                Err(e) => {
                    let msg = e;
                    self.keys.lock_key(&key_id, 400, msg.clone());
                    excluded.push(key_id.clone());
                    last_error = Some(msg.clone());
                    last_status = Some(400);
                    last_attempted_key_id = Some(key_id.clone());
                    failed.push(FailedKeyAttempt {
                        key_id: key_id.clone(),
                        error: GatewayError::ProviderError(msg),
                    });
                    continue;
                }
            };
            let cred = match self.refresh_if_needed(&key, cred).await {
                Ok(c) => c,
                Err(e) => { excluded.push(key_id.clone()); last_error = Some(e.to_string()); last_status = Some(502); last_attempted_key_id = Some(key_id.clone()); failed.push(FailedKeyAttempt { key_id: key_id.clone(), error: e }); continue; }
            };
            let body = self.mapper.to_responses_request(request.clone());
            match self.client.send_stream(body, &cred).await {
                Ok(stream) => {
                    self.keys.mark_success(&key_id);
                    return Ok(ChatStreamResult {
                        stream,
                        used_key_id: Some(key_id.clone()),
                        failed_keys: failed,
                        last_attempted_key_id: Some(key_id),
                    });
                }
                Err(e) => {
                    let msg = e.to_string();
                    let st = e.http_status().unwrap_or(502);
                    self.keys.lock_key(&key_id, st, msg.clone());
                    last_error = Some(msg.clone());
                    last_status = Some(st);
                    last_attempted_key_id = Some(key_id.clone());
                    failed.push(FailedKeyAttempt {
                        key_id: key_id.clone(),
                        error: e,
                    });
                    excluded.push(key_id);
                    continue;
                }
            }
        }
        Err(GatewayError::ProviderHttpError {
            status: last_status.unwrap_or(503),
            body: last_error.unwrap_or_default(),
            provider: self.metadata.name.clone(),
            key_id: last_attempted_key_id,
        })
    }

    async fn list_models(&self) -> Result<Vec<Model>, GatewayError> {
        let mut models = self.models_static();
        if let Some(pool) = &self.db {
            let custom = crate::db::list_custom_models(pool, &self.metadata.name).await;
            let prefix = self
                .metadata
                .model_prefix
                .clone()
                .unwrap_or_else(|| self.metadata.name.clone());
            for cm in custom {
                let id = format!("{}/{}", prefix, cm.model_id);
                if models.iter().any(|m| m.id == id) {
                    continue;
                }
                models.push(Model {
                    id,
                    object: "model".to_string(),
                    owned_by: self.metadata.display_name.clone(),
                    context_length: Some(cm.ctx as u32),
                });
            }
        }
        Ok(models)
    }
}

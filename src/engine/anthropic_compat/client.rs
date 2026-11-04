use std::sync::Arc;

use crate::engine::anthropic_compat::auth::ApiKeyAuth;
use crate::engine::anthropic_compat::config::AnthropicConfig;
use crate::engine::anthropic_compat::types::AnthropicRequest;
use crate::error::GatewayError;

pub struct Client {
    config: Arc<AnthropicConfig>,
    http: reqwest::Client,
}

impl Client {
    pub fn new(config: Arc<AnthropicConfig>) -> Self {
        let timeout = std::time::Duration::from_secs(config.default_timeout_secs.max(30));
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .user_agent("NuvCode/0.1.0")
            .build()
            .expect("reqwest Client::new");
        Self { config, http }
    }

    fn headers(&self, auth: &ApiKeyAuth) -> reqwest::header::HeaderMap {
        use reqwest::header::HeaderValue;
        let mut h = reqwest::header::HeaderMap::new();
        let (k, v) = auth.to_header(self.config.quirks.auth_header);
        h.insert(reqwest::header::HeaderName::from_bytes(k.as_bytes()).expect("auth header name"),
            HeaderValue::from_str(&v).expect("auth header value"));
        h.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        h.insert("content-type", HeaderValue::from_static("application/json"));
        h
    }

    /// Non-streaming POST /v1/messages
    pub async fn chat_non_streaming(
        &self,
        auth: &ApiKeyAuth,
        request: &AnthropicRequest,
    ) -> Result<crate::engine::anthropic_compat::types::AnthropicResponse, GatewayError> {
        let url = format!("{}/v1/messages", self.config.base_url);
        let body_str = serde_json::to_string(request).unwrap_or_default();
        tracing::debug!("[mmx] request body: {}", &body_str[..body_str.len().min(2000)]);
        let resp = self.http.post(&url)
            .headers(self.headers(auth))
            .json(request)
            .send()
            .await
            .map_err(|e| GatewayError::ProviderError(format!("HTTP request failed: {}", e)))?;

        let status = resp.status();
        let body = resp.text().await.map_err(|e| {
            GatewayError::ProviderError(format!("Failed to read response body: {}", e))
        })?;

        if !status.is_success() {
            return Err(GatewayError::ProviderError(format!(
                "Provider returned HTTP {}: {}",
                status.as_u16(), body
            )));
        }

        serde_json::from_str(&body).map_err(|e| {
            GatewayError::ProviderError(format!("Failed to parse Anthropic response: {} - body: {}", e, &body[..body.len().min(200)]))
        })
    }

    /// Streaming POST /v1/messages
    pub async fn chat_stream(
        &self,
        auth: &ApiKeyAuth,
        request: &AnthropicRequest,
    ) -> Result<reqwest::Response, GatewayError> {
        let url = format!("{}/v1/messages", self.config.base_url);
        let resp = self.http.post(&url)
            .headers(self.headers(auth))
            .json(request)
            .send()
            .await
            .map_err(|e| GatewayError::ProviderError(format!("HTTP request failed: {}", e)))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(GatewayError::ProviderError(format!(
                "Provider returned HTTP {}: {}",
                status.as_u16(), body
            )));
        }

        Ok(resp)
    }

    pub async fn validate_auth(&self, auth: &ApiKeyAuth) -> Result<(), GatewayError> {
        let url = format!("{}/v1/messages", self.config.base_url);
        // Lightweight validation: send a minimal request with max_tokens=1
        let body = serde_json::json!({
            "model": self.config.models.first().map(|m| m.id.as_str()).unwrap_or("unknown"),
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "hi"}]
        });

        let resp = self.http.post(&url)
            .headers(self.headers(auth))
            .json(&body)
            .send()
            .await
            .map_err(|e| GatewayError::ProviderError(format!("Auth validation failed: {}", e)))?;

        if resp.status().is_success() || resp.status().as_u16() == 400 {
            // 400 may mean actual auth works but request is too minimal
            Ok(())
        } else {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            Err(GatewayError::ProviderError(format!("Auth validation failed (HTTP {}): {}", status, body)))
        }
    }
}

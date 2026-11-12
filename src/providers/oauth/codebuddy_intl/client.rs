use crate::error::GatewayError;
use crate::types::chat::{ChatCompletionChunk, ChatCompletionResponse};
use futures::stream::{BoxStream, StreamExt};
use reqwest::Client;
use serde_json::Value;

use super::auth::CbaiOAuthCredential;
use super::constants;

fn uuid_v4_simple() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

pub struct CbaiClient {
    http: Client,
    first_chunk_timeout: std::time::Duration,
    stall_timeout: std::time::Duration,
}

impl CbaiClient {
    pub fn new() -> Self {
        Self {
            http: Client::builder()
                .connect_timeout(std::time::Duration::from_secs(constants::DEFAULT_TIMEOUT_SECS))
                .build()
                .expect("HTTP client"),
            first_chunk_timeout: std::time::Duration::from_secs(constants::STREAM_FIRST_CHUNK_TIMEOUT_SECS),
            stall_timeout: std::time::Duration::from_secs(constants::STREAM_STALL_TIMEOUT_SECS),
        }
    }

    fn headers(
        &self,
        builder: reqwest::RequestBuilder,
        cred: &CbaiOAuthCredential,
    ) -> reqwest::RequestBuilder {
        use std::time::{SystemTime, UNIX_EPOCH};

        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        let req_id = format!("{:x}", now.as_nanos());
        let conv_id = uuid_v4_simple();
        let conv_msg_id = uuid_v4_simple();
        let conv_req_id = uuid_v4_simple();

        let b = builder
            .header("Authorization", format!("Bearer {}", cred.access_token))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header("User-Agent", constants::USER_AGENT)
            .header("Host", "www.codebuddy.ai")
            .header("Origin", "https://www.codebuddy.cn")
            .header("Referer", "https://www.codebuddy.cn/")
            .header("X-Agent-Intent", "craft")
            .header("X-Client-Platform", "web")
            .header("X-Product", "SaaS")
            .header("X-Product-Version", constants::CLI_VERSION)
            .header("X-IDE-Name", "CLI")
            .header("X-IDE-Type", "CLI")
            .header("X-IDE-Version", constants::CLI_VERSION)
            .header("X-Request-ID", &req_id)
            .header("X-Conversation-ID", &conv_id)
            .header("X-Conversation-Message-ID", &conv_msg_id)
            .header("X-Conversation-Request-ID", &conv_req_id)
            .header("X-Requested-With", "XMLHttpRequest")
            .header("x-stainless-arch", std::env::consts::ARCH)
            .header("x-stainless-lang", "js")
            .header("x-stainless-os", std::env::consts::OS)
            .header("x-stainless-package-version", constants::CLI_VERSION)
            .header("x-stainless-retry-count", "0")
            .header("x-stainless-runtime", "node")
            .header("x-stainless-runtime-version", "v22.0.0");
        b
    }

    pub async fn send_collect(
        &self,
        body: Value,
        cred: &CbaiOAuthCredential,
    ) -> Result<ChatCompletionResponse, GatewayError> {
        if cred.access_token.is_empty() {
            return Err(GatewayError::ProviderError("CodeBuddy: access_token missing".into()));
        }
        let resp = self
            .headers(self.http.post(constants::CHAT_URL), cred)
            .json(&body)
            .send()
            .await
            .map_err(|e| GatewayError::ProviderError(format!("CodeBuddy HTTP: {e}")))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(GatewayError::ProviderHttpError {
                status: status.as_u16(),
                body: text,
                provider: constants::PROVIDER_ID.to_string(),
                key_id: None,
            });
        }
        if let Ok(response) = serde_json::from_str::<ChatCompletionResponse>(&text) {
            return Ok(response);
        }

        let mut id = String::new();
        let mut model = body["model"].as_str().unwrap_or_default().to_string();
        let mut created = 0;
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut finish_reason = None;
        let mut usage = None;
        for line in text.lines() {
            let Some(data) = line.trim().strip_prefix("data:") else { continue };
            let data = data.trim();
            if data == "[DONE]" { continue; }
            let Ok(chunk) = serde_json::from_str::<Value>(data) else { continue };
            id = chunk["id"].as_str().unwrap_or_default().to_string();
            model = chunk["model"].as_str().unwrap_or(&model).to_string();
            created = chunk["created"].as_u64().unwrap_or_default();
            if let Some(choice) = chunk["choices"].as_array().and_then(|a| a.first()) {
                let delta = &choice["delta"];
                content.push_str(delta["content"].as_str().unwrap_or_default());
                reasoning.push_str(delta["reasoning_content"].as_str().unwrap_or_default());
                finish_reason = choice["finish_reason"].as_str().map(str::to_string);
            }
        }
        if id.is_empty() {
            return Err(GatewayError::ProviderError(format!("CodeBuddy parse: expected JSON or SSE — body: {}", &text[..text.len().min(200)])));
        }
        let message = crate::types::chat::Message {
            role: "assistant".into(),
            content: Some(content),
            reasoning_content: (!reasoning.is_empty()).then_some(reasoning),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        };
        Ok(ChatCompletionResponse {
            id,
            object: "chat.completion".into(),
            created,
            model,
            choices: vec![crate::types::chat::Choice { index: 0, message, finish_reason }],
            usage,
        })
    }

    pub async fn send_stream(
        &self,
        body: Value,
        cred: &CbaiOAuthCredential,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, GatewayError>>, GatewayError> {
        if cred.access_token.is_empty() {
            return Err(GatewayError::ProviderError("CodeBuddy: access_token missing".into()));
        }
        let response = self
            .headers(self.http.post(constants::CHAT_URL), cred)
            .json(&body)
            .send()
            .await
            .map_err(|e| GatewayError::ProviderError(format!("CodeBuddy HTTP: {e}")))?;
        if !response.status().is_success() {
            let s = response.status().as_u16();
            let t = response.text().await.unwrap_or_default();
            return Err(GatewayError::ProviderHttpError {
                status: s,
                body: t,
                provider: constants::PROVIDER_ID.to_string(),
                key_id: None,
            });
        }

        let _model = body.get("model").and_then(|v| v.as_str()).unwrap_or("model").to_string();
        let mut upstream = response.bytes_stream();
        let ft = self.first_chunk_timeout;
        let st = self.stall_timeout;

        let stream = async_stream::try_stream! {
            let mut buf = String::new();
            let mut first = true;
            loop {
                let wait = if first { ft } else { st };
                first = false;
                let next = tokio::time::timeout(wait, upstream.next()).await
                    .map_err(|_| GatewayError::ProviderError(format!("CodeBuddy timeout: {}s", wait.as_secs())))?;
                let Some(bytes) = next else { break };
                let bytes = bytes.map_err(|e| GatewayError::ProviderError(format!("CodeBuddy read: {e}")))?;
                buf.push_str(&String::from_utf8_lossy(&bytes));
                while let Some(end) = buf.find("\n\n") {
                    let frame = buf[..end].to_string();
                    buf = buf[end + 2..].to_string();
                    for line in frame.lines() {
                        let Some(data) = line.trim().strip_prefix("data:") else { continue };
                        let data = data.trim();
                        if data == "[DONE]" { continue; }
                        match serde_json::from_str::<ChatCompletionChunk>(data) {
                            Ok(chunk) => yield chunk,
                            Err(e) => {
                                tracing::warn!("CodeBuddy SSE parse: {e}");
                                continue;
                            }
                        }
                    }
                }
            }
        };

        Ok(stream.boxed())
    }
}

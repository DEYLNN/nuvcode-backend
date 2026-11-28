use crate::error::GatewayError;
use crate::services::thinking_filter;
use crate::types::chat::{ChatCompletionChunk, Delta, ChunkChoice, Choice, ChatCompletionResponse, Message, ToolCall, Usage};

use futures::stream::{BoxStream, StreamExt};
use reqwest::Client;
use serde_json::Value;

use super::auth::ClCredential;
use super::constants;

pub struct ClClient {
    http: Client,
    first_chunk_timeout: std::time::Duration,
    stall_timeout: std::time::Duration,
}

impl ClClient {
    pub fn new() -> Self {
        Self {
            http: Client::builder()
                .connect_timeout(std::time::Duration::from_secs(constants::DEFAULT_TIMEOUT_SECS))
                .build()
                .expect("Failed to build HTTP client"),
            first_chunk_timeout: std::time::Duration::from_secs(constants::STREAM_FIRST_CHUNK_TIMEOUT_SECS),
            stall_timeout: std::time::Duration::from_secs(constants::STREAM_STALL_TIMEOUT_SECS),
        }
    }

    fn headers(&self, builder: reqwest::RequestBuilder, cred: &ClCredential) -> reqwest::RequestBuilder {
        builder
            .header("Authorization", format!("Bearer {}", cred.api_key))
            .header("Content-Type", "application/json")
            .header("User-Agent", constants::USER_AGENT)
    }

    pub async fn send_collect(&self, body: Value, cred: &ClCredential) -> Result<ChatCompletionResponse, GatewayError> {
        let url = format!("{}/v1/chat/completions", constants::BASE_URL);
        let response = self.headers(self.http.post(&url), cred)
            .json(&body)
            .send()
            .await
            .map_err(|e| GatewayError::ProviderError(format!("Cl HTTP: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            return Err(GatewayError::ProviderHttpError { status, body: text, provider: "cl".into(), key_id: None });
        }

        let json: Value = response.json().await
            .map_err(|e| GatewayError::ProviderError(format!("Cl parse: {}", e)))?;

        // Cline wraps response in {"data":{"choices":...}}
        let body_obj = if let Some(data) = json.get("data") { data } else { &json };

        let id = body_obj.get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("cl-unknown")
            .to_string();

        let model = body.get("model").and_then(|v| v.as_str()).unwrap_or("cl").to_string();
        let tags = constants::thinking_tags_for(&model);

        let reasoning_content = if model.contains(constants::HIDE_REASONING_MODEL) {
            None
        } else {
            body_obj.get("choices")
                .and_then(|c| c.as_array())
                .and_then(|arr| arr.first())
                .and_then(|c| c.get("message"))
                .and_then(|m| m.get("reasoning"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        };

        let tool_calls = body_obj.get("choices")
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("tool_calls"))
            .and_then(|v| serde_json::from_value::<Vec<ToolCall>>(v.clone()).ok());

        let finish = body_obj.get("choices")
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .and_then(|c| c.get("finish_reason"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Usage is at data.usage, not inside choices[]
        let usage = body_obj.get("usage");
        let usage_struct = usage.map(|u| Usage {
            prompt_tokens: u.get("prompt_tokens").and_then(|n| n.as_u64()).unwrap_or(0) as u32,
            completion_tokens: u.get("completion_tokens").and_then(|n| n.as_u64()).unwrap_or(0) as u32,
            total_tokens: u.get("total_tokens").and_then(|n| n.as_u64()).unwrap_or(0) as u32,
        });

        // Strip leaked agent thinking/tool-call tags from content (per-model config).
        let raw_content = body_obj.get("choices")
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let content = raw_content.map(|s| thinking_filter::strip_thinking_tags_const(&s, tags));

        Ok(ChatCompletionResponse {
            id,
            object: "chat.completion".to_string(),
            created: chrono::Utc::now().timestamp() as u64,
            model: body.get("model").and_then(|v| v.as_str()).unwrap_or("cl").to_string(),
            choices: vec![Choice {
                index: 0,
                message: Message {
                    role: "assistant".to_string(),
                    content,
                    tool_calls,
                    tool_call_id: None,
                    name: None,
                    reasoning_content,
                },
                finish_reason: finish.or(Some("stop".to_string())),
            }],
            usage: usage_struct.or(Some(Usage { prompt_tokens: 0, completion_tokens: 0, total_tokens: 0 })),
        })
    }

    pub async fn send_stream(&self, body: Value, cred: &ClCredential) -> Result<BoxStream<'static, Result<ChatCompletionChunk, GatewayError>>, GatewayError> {
        let url = format!("{}/v1/chat/completions", constants::BASE_URL);
        let response = self.headers(self.http.post(&url), cred)
            .json(&body)
            .send()
            .await
            .map_err(|e| GatewayError::ProviderError(format!("Cl HTTP: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            return Err(GatewayError::ProviderHttpError { status, body: text, provider: "cl".into(), key_id: None });
        }

        let model = body.get("model").and_then(|v| v.as_str()).unwrap_or("cl").to_string();
        let upstream = response.bytes_stream();

        let parsed = async_stream::try_stream! {
            let mut buffer = String::new();
            let mut content_buffer = String::new();
            let mut collected_usage: Option<Usage> = None;
            let first_chunk_timeout = std::time::Duration::from_secs(constants::STREAM_FIRST_CHUNK_TIMEOUT_SECS);
            let stall_timeout = std::time::Duration::from_secs(constants::STREAM_STALL_TIMEOUT_SECS);
            let mut first = true;
            futures::pin_mut!(upstream);
            loop {
                let wait = if first { first_chunk_timeout } else { stall_timeout };
                first = false;
                let next = tokio::time::timeout(wait, upstream.next()).await
                    .map_err(|_| GatewayError::ProviderError(format!("Cl stream timeout: {}s", wait.as_secs())))?;
                let Some(maybe_bytes) = next else { break; };
                let bytes = maybe_bytes.map_err(|e| GatewayError::ProviderError(format!("Cl stream read: {}", e)))?;
                buffer.push_str(&String::from_utf8_lossy(&bytes));
                while let Some(frame_end) = buffer.find("\n\n") {
                    let frame = buffer[..frame_end].to_string();
                    buffer = buffer[frame_end + 2..].to_string();
                    for line in frame.lines() {
                        let Some(data) = line.trim().strip_prefix("data:") else { continue; };
                        let data = data.trim();
                        if data.is_empty() || data == "[DONE]" { continue; }
                        if let Ok(v) = serde_json::from_str::<Value>(data) {
                            let body_obj = if let Some(d) = v.get("data") { d } else { &v };
                            if let Some(chunk) = Self::parse_chunk(body_obj, &model, &mut collected_usage, &mut content_buffer) {
                                yield chunk;
                            }
                        }
                    }
                }
            }
            // Flush collected usage in a terminal chunk so the gateway persists
            // token counts (upstream sends usage on the choices:[] chunk, which
            // parse_chunk collects but does not yield).
            if let Some(u) = collected_usage.take() {
                yield ChatCompletionChunk {
                    id: format!("chatcmpl-cl-{}", chrono::Utc::now().timestamp()),
                    object: "chat.completion.chunk".to_string(),
                    created: chrono::Utc::now().timestamp() as u64,
                    model: model.to_string(),
                    choices: vec![],
                    usage: Some(u),
                };
            }
        };
        Ok(parsed.boxed())
    }

    fn parse_chunk(v: &Value, model: &str, usage: &mut Option<Usage>, content_buffer: &mut String) -> Option<ChatCompletionChunk> {
        let choices = v.get("choices").and_then(|c| c.as_array()).cloned().unwrap_or_default();
        if choices.is_empty() {
            if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
                *usage = Some(Usage {
                    prompt_tokens: u.get("prompt_tokens").and_then(|n| n.as_u64()).unwrap_or(0) as u32,
                    completion_tokens: u.get("completion_tokens").and_then(|n| n.as_u64()).unwrap_or(0) as u32,
                    total_tokens: u.get("total_tokens").and_then(|n| n.as_u64()).unwrap_or(0) as u32,
                });
            }
            return None;
        }
        let choice = &choices[0];
        let idx = choice.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
        let delta = choice.get("delta").cloned().unwrap_or_default();
        // Strip leaked agent thinking/tool-call tags from streaming content too.
        let tags = constants::thinking_tags_for(model);
        if let Some(raw) = delta.get("content").and_then(|c| c.as_str()) {
            content_buffer.push_str(raw);
        }
        let reasoning_content = if model.contains(constants::HIDE_REASONING_MODEL) {
            None
        } else {
            delta.get("reasoning").and_then(|c| c.as_str()).map(|s| s.to_string())
        };
        let finish = choice.get("finish_reason").and_then(|f| f.as_str()).map(|s| s.to_string());

        // Tool calls in streaming delta (ChunkToolCall — index-based)
        let tool_calls = delta.get("tool_calls")
            .and_then(|tc| serde_json::from_value::<Vec<crate::types::chat::ChunkToolCall>>(tc.clone()).ok());

        // Extract usage from provider_metadata (Cline-specific: delta.provider_metadata.deepseek)
        if let Some(pm) = delta.get("provider_metadata") {
            if let Some(ds) = pm.get("deepseek") {
                let pt = ds.get("promptCacheHitTokens").and_then(|n| n.as_u64()).unwrap_or(0)
                    + ds.get("promptCacheMissTokens").and_then(|n| n.as_u64()).unwrap_or(0);
                let full_usage = Usage {
                    prompt_tokens: pt as u32,
                    completion_tokens: 0,
                    total_tokens: pt as u32,
                };
                if pt > 0 { *usage = Some(full_usage); }
            }
        }
        // Also check for usage in the top-level v (some providers send it)
        if usage.is_none() {
            if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
                *usage = Some(Usage {
                    prompt_tokens: u.get("prompt_tokens").and_then(|n| n.as_u64()).unwrap_or(0) as u32,
                    completion_tokens: u.get("completion_tokens").and_then(|n| n.as_u64()).unwrap_or(0) as u32,
                    total_tokens: u.get("total_tokens").and_then(|n| n.as_u64()).unwrap_or(0) as u32,
                });
            }
        }

        // Always yield if we have content, reasoning, finish_reason, or tool_calls
        let content = if finish.is_some() {
            let full = std::mem::take(content_buffer);
            let filtered = thinking_filter::strip_thinking_tags_const(&full, tags);
            (!filtered.is_empty()).then_some(filtered)
        } else {
            None
        };
        let has_content = content.is_some();
        let has_reasoning = reasoning_content.is_some();
        let has_finish = finish.is_some();
        let has_tool_calls = tool_calls.is_some();
        if !has_content && !has_reasoning && !has_finish && !has_tool_calls { return None; }

        Some(ChatCompletionChunk {
            id: format!("chatcmpl-cl-{}", chrono::Utc::now().timestamp()),
            object: "chat.completion.chunk".to_string(),
            created: chrono::Utc::now().timestamp() as u64,
            model: model.to_string(),
            choices: vec![ChunkChoice {
                index: idx,
                delta: Delta { role: None, content, reasoning_content, tool_calls },
                finish_reason: finish,
            }],
            usage: usage.clone(),
        })
    }
}

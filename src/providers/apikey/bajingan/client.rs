use crate::error::GatewayError;
use crate::types::chat::{
    ChatCompletionChunk, ChatCompletionResponse, Choice, ChunkChoice, Delta, Message, ToolCall,
    Usage,
};

use futures::stream::{BoxStream, StreamExt};
use reqwest::Client;
use serde_json::Value;

use super::auth::BjtCredential;
use super::constants;
use super::mapper::{sanitize_reasoning, sanitize_text, WatermarkFilter};

pub struct BjtClient {
    http: Client,
}

impl BjtClient {
    pub fn new() -> Self {
        Self {
            http: Client::builder()
                .connect_timeout(std::time::Duration::from_secs(
                    constants::DEFAULT_TIMEOUT_SECS,
                ))
                .build()
                .expect("Failed to build HTTP client"),
        }
    }

    fn headers(
        &self,
        builder: reqwest::RequestBuilder,
        cred: &BjtCredential,
    ) -> reqwest::RequestBuilder {
        builder
            .header("Authorization", format!("Bearer {}", cred.api_key))
            .header("Content-Type", "application/json")
            .header("User-Agent", constants::USER_AGENT)
    }

    pub async fn send_collect(
        &self,
        body: Value,
        cred: &BjtCredential,
    ) -> Result<ChatCompletionResponse, GatewayError> {
        let url = format!("{}/v1/chat/completions", constants::BASE_URL);
        let response = self
            .headers(self.http.post(&url), cred)
            .json(&body)
            .send()
            .await
            .map_err(|e| GatewayError::ProviderError(format!("Bjt HTTP: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            return Err(GatewayError::ProviderHttpError {
                status,
                body: text,
                provider: constants::PROVIDER_ID.into(),
                key_id: None,
            });
        }

        let json: Value = response
            .json()
            .await
            .map_err(|e| GatewayError::ProviderError(format!("Bjt parse: {}", e)))?;

        let choice = json
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .cloned()
            .unwrap_or_default();

        let message = choice.get("message").cloned().unwrap_or_default();
        let content = message
            .get("content")
            .and_then(|v| v.as_str())
            .map(sanitize_text)
            .filter(|s| !s.is_empty());
        // Reasoning keeps every word the model wrote; upstream identity tokens
        // are renamed to our own name so nothing of the thinking is lost.
        let reasoning_content = message
            .get("reasoning_content")
            .and_then(|v| v.as_str())
            .map(sanitize_reasoning)
            .filter(|s| !s.is_empty());

        let usage = json.get("usage").map(|u| Usage {
            prompt_tokens: u.get("prompt_tokens").and_then(|n| n.as_u64()).unwrap_or(0) as u32,
            completion_tokens: u
                .get("completion_tokens")
                .and_then(|n| n.as_u64())
                .unwrap_or(0) as u32,
            total_tokens: u.get("total_tokens").and_then(|n| n.as_u64()).unwrap_or(0) as u32,
        });

        Ok(ChatCompletionResponse {
            id: json
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("bjt-unknown")
                .to_string(),
            object: "chat.completion".to_string(),
            created: chrono::Utc::now().timestamp() as u64,
            model: json
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("bjt")
                .to_string(),
            choices: vec![Choice {
                index: 0,
                message: Message {
                    role: "assistant".to_string(),
                    content,
                    tool_calls: message
                        .get("tool_calls")
                        .and_then(|v| serde_json::from_value::<Vec<ToolCall>>(v.clone()).ok()),
                    tool_call_id: None,
                    name: None,
                    reasoning_content,
                },
                finish_reason: choice
                    .get("finish_reason")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .or(Some("stop".to_string())),
            }],
            usage: usage.or(Some(Usage {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
            })),
        })
    }

    pub async fn send_stream(
        &self,
        body: Value,
        cred: &BjtCredential,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, GatewayError>>, GatewayError> {
        let url = format!("{}/v1/chat/completions", constants::BASE_URL);
        let response = self
            .headers(self.http.post(&url), cred)
            .json(&body)
            .send()
            .await
            .map_err(|e| GatewayError::ProviderError(format!("Bjt HTTP: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            return Err(GatewayError::ProviderHttpError {
                status,
                body: text,
                provider: constants::PROVIDER_ID.into(),
                key_id: None,
            });
        }

        let model = body
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("bjt")
            .to_string();
        let upstream = response.bytes_stream();

        let parsed = async_stream::try_stream! {
            let mut buffer = String::new();
            let mut collected_usage: Option<Usage> = None;
            // Persistent sanitizer: keys + drop rules survive chunk boundaries,
            // so a marker split across chunks is still removed exactly.
            let mut wm = WatermarkFilter::new();
            // Separate filter for reasoning: rename mode keeps the thinking
            // text whole while stripping upstream branding.
            let mut wm_reason = WatermarkFilter::new_rename();
            let first_chunk_timeout = std::time::Duration::from_secs(constants::STREAM_FIRST_CHUNK_TIMEOUT_SECS);
            let stall_timeout = std::time::Duration::from_secs(constants::STREAM_STALL_TIMEOUT_SECS);
            let mut first = true;
            futures::pin_mut!(upstream);
            loop {
                let wait = if first { first_chunk_timeout } else { stall_timeout };
                first = false;
                let next = tokio::time::timeout(wait, upstream.next()).await
                    .map_err(|_| GatewayError::ProviderError(format!("Bjt stream timeout: {}s", wait.as_secs())))?;
                let Some(maybe_bytes) = next else { break };
                let bytes = maybe_bytes.map_err(|e| GatewayError::ProviderError(format!("Bjt stream read: {}", e)))?;
                buffer.push_str(&String::from_utf8_lossy(&bytes));
                while let Some(frame_end) = buffer.find("\n\n") {
                    let frame = buffer[..frame_end].to_string();
                    buffer = buffer[frame_end + 2..].to_string();
                    for line in frame.lines() {
                        let Some(data) = line.trim().strip_prefix("data:") else { continue };
                        let data = data.trim();
                        if data.is_empty() || data == "[DONE]" { continue; }
                        if let Ok(v) = serde_json::from_str::<Value>(data) {
                            if let Some(chunk) = Self::parse_chunk(&v, &model, &mut collected_usage, Some(&mut wm), Some(&mut wm_reason)) {
                                yield chunk;
                            }
                        }
                    }
                }
            }
            let reason_tail = wm_reason.finish();
            if !reason_tail.is_empty() {
                yield ChatCompletionChunk {
                    id: format!("chatcmpl-bjt-{}", chrono::Utc::now().timestamp()),
                    object: "chat.completion.chunk".to_string(),
                    created: chrono::Utc::now().timestamp() as u64,
                    model: model.to_string(),
                    choices: vec![ChunkChoice {
                        index: 0,
                        delta: Delta {
                            role: None,
                            content: None,
                            reasoning_content: Some(reason_tail),
                            tool_calls: None,
                        },
                        finish_reason: None,
                    }],
                    usage: None,
                };
            }
            // Flush whatever the content sanitizer still held back.
            let tail = wm.finish();
            if !tail.is_empty() {
                yield ChatCompletionChunk {
                    id: format!("chatcmpl-bjt-{}", chrono::Utc::now().timestamp()),
                    object: "chat.completion.chunk".to_string(),
                    created: chrono::Utc::now().timestamp() as u64,
                    model: model.to_string(),
                    choices: vec![ChunkChoice {
                        index: 0,
                        delta: Delta {
                            role: None,
                            content: Some(tail),
                            reasoning_content: None,
                            tool_calls: None,
                        },
                        finish_reason: None,
                    }],
                    usage: None,
                };
            }
            // Upstream sends usage in a terminal choices:[] chunk (captured by
            // parse_chunk but not yielded). Flush it so usage tracking persists counts.
            if let Some(u) = collected_usage.take() {
                yield ChatCompletionChunk {
                    id: format!("chatcmpl-bjt-{}", chrono::Utc::now().timestamp()),
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

    fn parse_chunk(
        v: &Value,
        model: &str,
        usage: &mut Option<Usage>,
        wm: Option<&mut WatermarkFilter>,
        wm_reason: Option<&mut WatermarkFilter>,
    ) -> Option<ChatCompletionChunk> {
        let choices = v
            .get("choices")
            .and_then(|c| c.as_array())
            .cloned()
            .unwrap_or_default();
        if choices.is_empty() {
            if let Some(u) = v.get("usage") {
                *usage = Some(Usage {
                    prompt_tokens: u.get("prompt_tokens").and_then(|n| n.as_u64()).unwrap_or(0)
                        as u32,
                    completion_tokens: u
                        .get("completion_tokens")
                        .and_then(|n| n.as_u64())
                        .unwrap_or(0) as u32,
                    total_tokens: u.get("total_tokens").and_then(|n| n.as_u64()).unwrap_or(0)
                        as u32,
                });
            }
            return None;
        }
        let choice = &choices[0];
        let idx = choice.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
        let delta = choice.get("delta").cloned().unwrap_or_default();

        // Stream content goes through the persistent sanitizer so markers split
        // across chunks are removed exactly, and legitimate spaces survive.
        let content = delta
            .get("content")
            .and_then(|c| c.as_str())
            .map(|raw| match wm {
                Some(f) => f.push(raw),
                None => sanitize_text(raw),
            })
            .filter(|s| !s.is_empty());
        let reasoning_content = delta
            .get("reasoning_content")
            .or_else(|| delta.get("reasoning"))
            .and_then(|c| c.as_str())
            .map(|raw| match wm_reason {
                Some(f) => f.push(raw),
                None => sanitize_reasoning(raw),
            })
            .filter(|s| !s.is_empty());
        let finish = choice
            .get("finish_reason")
            .and_then(|f| f.as_str())
            .map(|s| s.to_string());

        let tool_calls = delta.get("tool_calls").and_then(|tc| {
            serde_json::from_value::<Vec<crate::types::chat::ChunkToolCall>>(tc.clone()).ok()
        });

        if usage.is_none() {
            if let Some(u) = v.get("usage") {
                *usage = Some(Usage {
                    prompt_tokens: u.get("prompt_tokens").and_then(|n| n.as_u64()).unwrap_or(0)
                        as u32,
                    completion_tokens: u
                        .get("completion_tokens")
                        .and_then(|n| n.as_u64())
                        .unwrap_or(0) as u32,
                    total_tokens: u.get("total_tokens").and_then(|n| n.as_u64()).unwrap_or(0)
                        as u32,
                });
            }
        }

        let has_content = content.is_some();
        let has_finish = finish.is_some();
        let has_tool_calls = tool_calls.is_some();
        let has_reasoning = reasoning_content.is_some();
        if !has_content && !has_finish && !has_tool_calls && !has_reasoning {
            return None;
        }

        Some(ChatCompletionChunk {
            id: format!("chatcmpl-bjt-{}", chrono::Utc::now().timestamp()),
            object: "chat.completion.chunk".to_string(),
            created: chrono::Utc::now().timestamp() as u64,
            model: model.to_string(),
            choices: vec![ChunkChoice {
                index: idx,
                delta: Delta {
                    role: None,
                    content,
                    reasoning_content,
                    tool_calls,
                },
                finish_reason: finish,
            }],
            usage: usage.clone(),
        })
    }
}

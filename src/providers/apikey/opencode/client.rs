use crate::error::GatewayError;
use crate::services::thinking_filter;
use crate::types::chat::{
    ChatCompletionChunk, ChatCompletionResponse, Choice, ChunkChoice, Delta, Message, ToolCall,
    Usage,
};

use futures::stream::{BoxStream, StreamExt};
use reqwest::Client;
use serde_json::Value;

use super::auth::OcfCredential;
use super::constants;

pub struct OcfClient {
    http: Client,
}

impl OcfClient {
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

    /// Generate session ID: `ses_<12 hex><14 base62>`
    fn gen_session_id() -> String {
        format!("ses_{}{}", hex12(), base62_14())
    }

    /// Generate request ID: `msg_<12 hex><14 base62>`
    fn gen_request_id() -> String {
        format!("msg_{}{}", hex12(), base62_14())
    }

    fn headers(
        &self,
        builder: reqwest::RequestBuilder,
        _cred: &OcfCredential,
    ) -> reqwest::RequestBuilder {
        builder
            .header("Authorization", "Bearer public")
            .header("Content-Type", "application/json")
            .header("User-Agent", constants::USER_AGENT)
            .header("x-opencode-client", "desktop")
            .header("x-opencode-session", Self::gen_session_id())
            .header("x-opencode-request", Self::gen_request_id())
            .header("x-opencode-project", "global")
    }

    /// Per-model endpoint routing.
    fn endpoint_url(model: &str) -> String {
        if model.contains("muse-spark") {
            format!("{}/v1/responses", constants::BASE_URL)
        } else if model == "union-alpha" {
            format!("{}/v1/messages", constants::BASE_URL)
        } else {
            format!("{}/v1/chat/completions", constants::BASE_URL)
        }
    }

    pub async fn send_collect(
        &self,
        body: Value,
        cred: &OcfCredential,
    ) -> Result<ChatCompletionResponse, GatewayError> {
        let model = body
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let url = Self::endpoint_url(&model);
        let http = self.http.clone();
        let response = self
            .headers(http.post(&url), cred)
            .json(&body)
            .send()
            .await
            .map_err(|e| GatewayError::ProviderError(format!("Ocf HTTP: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            return Err(GatewayError::ProviderHttpError {
                status,
                body: text,
                provider: "ocf".into(),
                key_id: None,
            });
        }

        // Upstream always returns SSE (stream:true forced by build_body).
        // Parse SSE text instead of JSON.
        let raw = response
            .text()
            .await
            .map_err(|e| GatewayError::ProviderError(format!("Ocf read: {}", e)))?;

        let mut content = String::new();
        let mut usage: Option<Usage> = None;
        let mut id = String::new();
        let mut finish_reason = "stop".to_string();
        let mut tool_calls: Option<Vec<ToolCall>> = None;

        for line in raw.lines() {
            let Some(data) = line.trim().strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(data) else {
                continue;
            };

            if id.is_empty() {
                if let Some(chunk_id) = v.get("id").and_then(|i| i.as_str()) {
                    id = chunk_id.to_string();
                }
            }

            if let Some(u) = v.get("usage") {
                usage = Some(Usage {
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

            let Some(choices) = v.get("choices").and_then(|c| c.as_array()) else {
                continue;
            };
            if choices.is_empty() {
                continue;
            }
            let Some(choice) = choices.first() else {
                continue;
            };
            let delta = choice.get("delta").cloned().unwrap_or_default();

            if let Some(c) = delta.get("content").and_then(|c| c.as_str()) {
                content.push_str(c);
            }
            // reasoning_content dropped — thinking hidden downstream, same as
            // send_stream's parse_chunk.
            if let Some(tc) = delta.get("tool_calls") {
                if let Ok(parsed) = serde_json::from_value::<Vec<ToolCall>>(tc.clone()) {
                    tool_calls = Some(parsed);
                }
            }
            if let Some(fr) = choice.get("finish_reason").and_then(|f| f.as_str()) {
                finish_reason = fr.to_string();
            }
        }

        let content =
            thinking_filter::strip_thinking_tags_const(&content, constants::THINKING_TAGS);
        let content = if content.is_empty() { None } else { Some(content) };

        Ok(ChatCompletionResponse {
            id: if id.is_empty() { "ocf-unknown".to_string() } else { id },
            object: "chat.completion".to_string(),
            created: chrono::Utc::now().timestamp() as u64,
            model,
            choices: vec![Choice {
                index: 0,
                message: Message {
                    role: "assistant".to_string(),
                    content,
                    tool_calls,
                    tool_call_id: None,
                    name: None,
                    reasoning_content: None,
                },
                finish_reason: Some(finish_reason),
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
        cred: &OcfCredential,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, GatewayError>>, GatewayError> {
        let model = body
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("ocf")
            .to_string();
        let url = Self::endpoint_url(&model);
        let http = self.http.clone();
        let response = self
            .headers(http.post(&url), cred)
            .json(&body)
            .send()
            .await
            .map_err(|e| GatewayError::ProviderError(format!("Ocf HTTP: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            return Err(GatewayError::ProviderHttpError {
                status,
                body: text,
                provider: "ocf".into(),
                key_id: None,
            });
        }

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
                    .map_err(|_| GatewayError::ProviderError(format!("Ocf stream timeout: {}s", wait.as_secs())))?;
                let Some(maybe_bytes) = next else { break };
                let bytes = maybe_bytes.map_err(|e| GatewayError::ProviderError(format!("Ocf stream read: {}", e)))?;
                buffer.push_str(&String::from_utf8_lossy(&bytes));
                while let Some(frame_end) = buffer.find("\n\n") {
                    let frame = buffer[..frame_end].to_string();
                    buffer = buffer[frame_end + 2..].to_string();
                    for line in frame.lines() {
                        let Some(data) = line.trim().strip_prefix("data:") else { continue };
                        let data = data.trim();
                        if data.is_empty() || data == "[DONE]" { continue; }
                        if let Ok(v) = serde_json::from_str::<Value>(data) {
                            if let Some(chunk) = Self::parse_chunk(&v, &model, &mut collected_usage, &mut content_buffer) {
                                yield chunk;
                            }
                        }
                    }
                }
            }
            if let Some(u) = collected_usage.take() {
                yield ChatCompletionChunk {
                    id: format!("chatcmpl-ocf-{}", chrono::Utc::now().timestamp()),
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
        content_buffer: &mut String,
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
        let _ = delta
            .get("reasoning_content")
            .or_else(|| delta.get("reasoning"));
        if let Some(raw) = delta.get("content").and_then(|c| c.as_str()) {
            content_buffer.push_str(raw);
        }
        let finish = choice
            .get("finish_reason")
            .and_then(|f| f.as_str())
            .map(|s| s.to_string());

        let tool_calls = delta.get("tool_calls").and_then(|tc| {
            serde_json::from_value::<Vec<crate::types::chat::ChunkToolCall>>(tc.clone()).ok()
        });

        // Only collect usage from chunks — don't attach to yielded chunk.
        // The final chunk (choices empty) sets usage, then we yield it separately.
        if let Some(u) = v.get("usage") {
            let new_usage = Usage {
                prompt_tokens: u.get("prompt_tokens").and_then(|n| n.as_u64()).unwrap_or(0)
                    as u32,
                completion_tokens: u
                    .get("completion_tokens")
                    .and_then(|n| n.as_u64())
                    .unwrap_or(0) as u32,
                total_tokens: u.get("total_tokens").and_then(|n| n.as_u64()).unwrap_or(0)
                    as u32,
            };
            // Only overwrite if new usage has real values (non-zero)
            if new_usage.total_tokens > 0 || usage.is_none() {
                *usage = Some(new_usage);
            }
        }

        let content = if finish.is_some() {
            let full = std::mem::take(content_buffer);
            let filtered =
                thinking_filter::strip_thinking_tags_const(&full, constants::THINKING_TAGS);
            (!filtered.is_empty()).then_some(filtered)
        } else {
            None
        };
        let has_content = content.is_some();
        let has_finish = finish.is_some();
        let has_tool_calls = tool_calls.is_some();
        if !has_content && !has_finish && !has_tool_calls {
            return None;
        }

        Some(ChatCompletionChunk {
            id: format!("chatcmpl-ocf-{}", chrono::Utc::now().timestamp()),
            object: "chat.completion.chunk".to_string(),
            created: chrono::Utc::now().timestamp() as u64,
            model: model.to_string(),
            choices: vec![ChunkChoice {
                index: idx,
                delta: Delta {
                    role: None,
                    content,
                    reasoning_content: None,
                    tool_calls,
                },
                finish_reason: finish,
            }],
            usage: None, // Don't attach usage to content chunks — only final chunk gets usage
        })
    }
}

// --- ID generation helpers ---

/// 12 hex chars from timestamp (lower 48 bits → 12 hex digits)
fn hex12() -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("{:012x}", ts & 0xFFFF_FFFF_FFFF)
}

const BASE62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// 14 base62 chars from a PRNG seeded by time + thread id
fn base62_14() -> String {
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let mut state = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    (0..14)
        .map(|_| {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let idx = ((state >> 33) as usize) % BASE62.len();
            BASE62[idx] as char
        })
        .collect()
}

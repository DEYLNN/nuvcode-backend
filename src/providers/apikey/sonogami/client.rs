use crate::error::GatewayError;
use crate::services::thinking_filter;
use crate::types::chat::{
    ChatCompletionChunk, ChatCompletionResponse, Choice, ChunkChoice, Delta, Message, ToolCall,
    Usage,
};

use futures::stream::{BoxStream, StreamExt};
use reqwest::Client;
use serde_json::Value;

use super::auth::SonoCredential;
use super::constants;

pub struct SonoClient {
    http: Client,
}

impl SonoClient {
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
        cred: &SonoCredential,
    ) -> reqwest::RequestBuilder {
        builder
            .header("Authorization", format!("Bearer {}", cred.api_key))
            .header("Content-Type", "application/json")
            .header("User-Agent", constants::USER_AGENT)
    }

    pub async fn send_collect(
        &self,
        body: Value,
        cred: &SonoCredential,
    ) -> Result<ChatCompletionResponse, GatewayError> {
        let url = format!("{}/chat/completions", constants::BASE_URL);
        let response = self
            .headers(self.http.post(&url), cred)
            .json(&body)
            .send()
            .await
            .map_err(|e| GatewayError::ProviderError(format!("Sono HTTP: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            return Err(GatewayError::ProviderHttpError {
                status,
                body: text,
                provider: "sono".into(),
                key_id: None,
            });
        }

        let json: Value = response
            .json()
            .await
            .map_err(|e| GatewayError::ProviderError(format!("Sono parse: {}", e)))?;

        let choice = json
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .cloned()
            .unwrap_or_default();

        let message = choice.get("message").cloned().unwrap_or_default();
        // FusionCode models always think — drop reasoning_content, strip think-block tags.
        let raw_content = message
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let content =
            thinking_filter::strip_thinking_tags_const(&raw_content, constants::THINKING_TAGS);
        let content = Self::strip_watermark(&content);
        let content = if content.is_empty() {
            None
        } else {
            Some(content)
        };

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
                .unwrap_or("sono-unknown")
                .to_string(),
            object: "chat.completion".to_string(),
            created: chrono::Utc::now().timestamp() as u64,
            model: json
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("sono")
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
                    reasoning_content: None, // hidden
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
        cred: &SonoCredential,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, GatewayError>>, GatewayError> {
        let url = format!("{}/chat/completions", constants::BASE_URL);
        let response = self
            .headers(self.http.post(&url), cred)
            .json(&body)
            .send()
            .await
            .map_err(|e| GatewayError::ProviderError(format!("Sono HTTP: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            return Err(GatewayError::ProviderHttpError {
                status,
                body: text,
                provider: "sono".into(),
                key_id: None,
            });
        }

        let model = body
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("sono")
            .to_string();
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
                    .map_err(|_| GatewayError::ProviderError(format!("Sono stream timeout: {}s", wait.as_secs())))?;
                let Some(maybe_bytes) = next else { break };
                let bytes = maybe_bytes.map_err(|e| GatewayError::ProviderError(format!("Sono stream read: {}", e)))?;
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
            // Upstream sends usage in a terminal choices:[] chunk (captured by
            // parse_chunk but not yielded). Flush it as the final chunk so the
            // dispatcher's usage-tracking can persist token counts.
            if let Some(u) = collected_usage.take() {
                yield ChatCompletionChunk {
                    id: format!("chatcmpl-sono-{}", chrono::Utc::now().timestamp()),
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
            if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
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
        // reasoning_content / reasoning chunks dropped entirely — thinking hidden downstream.
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

        if usage.is_none() {
            if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
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

        // Buffer-then-strip on finish: think blocks spanning multiple chunks removed cleanly.
        let content = if finish.is_some() {
            let full = std::mem::take(content_buffer);
            let filtered =
                thinking_filter::strip_thinking_tags_const(&full, constants::THINKING_TAGS);
            let filtered = Self::strip_watermark(&filtered);
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
            id: format!("chatcmpl-sono-{}", chrono::Utc::now().timestamp()),
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

    /// Strip Sonogami router watermark from content.
    /// Pattern: zero-width chars + "⚠️ This model is FREE and served by telegram @nookiesol..."
    /// wrapped in invisible unicode (U+2060 word joiner + zero-width chars).
    fn strip_watermark(content: &str) -> String {
        // Remove the visible watermark text and everything after it (including wrapping zero-width chars)
        if let Some(pos) = content.find("⚠️ This model is FREE") {
            // Walk backwards from pos to catch leading zero-width/invisible chars
            let start = content[..pos]
                .char_indices()
                .rev()
                .take_while(|(_, c)| {
                    *c == '\u{2060}' // word joiner
                        || *c == '\u{200B}' // zero-width space
                        || *c == '\u{200C}' // zero-width non-joiner
                        || *c == '\u{200D}' // zero-width joiner
                        || *c == '\u{FEFF}' // BOM
                        || *c == '\u{2060}'
                        || c.is_control()
                        || (*c as u32 == 0x2060)
                })
                .last()
                .map(|(i, _)| i)
                .unwrap_or(pos);
            // Walk forward past the watermark to catch trailing zero-width chars
            let end_marker = "demand a refund, stop using that provider.";
            if let Some(end_pos) = content.find(end_marker) {
                let after = end_pos + end_marker.len();
                // Skip trailing zero-width chars after the watermark text
                let trail = content[after..]
                    .char_indices()
                    .take_while(|(_, c)| {
                        *c == '\u{2060}'
                            || *c == '\u{200B}'
                            || *c == '\u{200C}'
                            || *c == '\u{200D}'
                            || *c == '\u{FEFF}'
                            || c.is_control()
                    })
                    .last()
                    .map(|(i, _)| after + i + content[after..].chars().next().unwrap().len_utf8())
                    .unwrap_or(after);
                // Reconstruct: before watermark start + after watermark end
                let before = content[..start].trim_end();
                let rest = content[trail..].trim_start();
                if rest.is_empty() {
                    return before.to_string();
                }
                return format!("{}\n{}", before, rest);
            }
            // Fallback: just cut everything from the zero-width prefix
            return content[..start].trim_end().to_string();
        }
        content.to_string()
    }
}

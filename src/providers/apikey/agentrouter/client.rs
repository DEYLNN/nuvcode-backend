use crate::error::GatewayError;
use crate::types::chat::{
    ChatCompletionChunk, ChatCompletionResponse, Choice, ChunkChoice, Delta, Message, ToolCall,
    Usage,
};

use futures::stream::{BoxStream, StreamExt};
use reqwest::Client;
use serde_json::Value;

use super::auth::AgrCredential;
use super::constants;

/// Check if the model (after stripping "agr/" prefix) is in
/// `RESPONSES_ONLY_MODELS`, meaning it only supports `/v1/responses`.
fn is_responses_model(model: &str) -> bool {
    let stripped = model.strip_prefix("agr/").unwrap_or(model);
    constants::RESPONSES_ONLY_MODELS.iter().any(|m| *m == stripped)
}

pub struct AgrClient {
    http: Client,
}

impl AgrClient {
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
        cred: &AgrCredential,
    ) -> reqwest::RequestBuilder {
        builder
            .header("Authorization", format!("Bearer {}", cred.api_key))
            .header("Content-Type", "application/json")
            .header("User-Agent", constants::USER_AGENT)
    }

    pub async fn send_collect(
        &self,
        body: Value,
        cred: &AgrCredential,
    ) -> Result<ChatCompletionResponse, GatewayError> {
        let model = body
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if is_responses_model(model) {
            return self.send_collect_responses(body, cred).await;
        }

        let url = format!("{}/chat/completions", constants::BASE_URL);
        let response = self
            .headers(self.http.post(&url), cred)
            .json(&body)
            .send()
            .await
            .map_err(|e| GatewayError::ProviderError(format!("Agr HTTP: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            return Err(GatewayError::ProviderHttpError {
                status,
                body: text,
                provider: "agr".into(),
                key_id: None,
            });
        }

        let json: Value = response
            .json()
            .await
            .map_err(|e| GatewayError::ProviderError(format!("Agr parse: {}", e)))?;

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
            .map(|s| s.to_string());

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
                .unwrap_or("agr-unknown")
                .to_string(),
            object: "chat.completion".to_string(),
            created: chrono::Utc::now().timestamp() as u64,
            model: json
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("agr")
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
                    reasoning_content: message
                        .get("reasoning_content")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string()),
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
        cred: &AgrCredential,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, GatewayError>>, GatewayError> {
        let model = body
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if is_responses_model(model) {
            return self.send_stream_responses(body, cred).await;
        }

        let url = format!("{}/chat/completions", constants::BASE_URL);
        let response = self
            .headers(self.http.post(&url), cred)
            .json(&body)
            .send()
            .await
            .map_err(|e| GatewayError::ProviderError(format!("Agr HTTP: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            return Err(GatewayError::ProviderHttpError {
                status,
                body: text,
                provider: "agr".into(),
                key_id: None,
            });
        }

        let model = body
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("agr")
            .to_string();
        let upstream = response.bytes_stream();

        let parsed = async_stream::try_stream! {
            let mut buffer = String::new();
            let mut collected_usage: Option<Usage> = None;
            let first_chunk_timeout = std::time::Duration::from_secs(constants::STREAM_FIRST_CHUNK_TIMEOUT_SECS);
            let stall_timeout = std::time::Duration::from_secs(constants::STREAM_STALL_TIMEOUT_SECS);
            let mut first = true;
            futures::pin_mut!(upstream);
            loop {
                let wait = if first { first_chunk_timeout } else { stall_timeout };
                first = false;
                let next = tokio::time::timeout(wait, upstream.next()).await
                    .map_err(|_| GatewayError::ProviderError(format!("Agr stream timeout: {}s", wait.as_secs())))?;
                let Some(maybe_bytes) = next else { break };
                let bytes = maybe_bytes.map_err(|e| GatewayError::ProviderError(format!("Agr stream read: {}", e)))?;
                buffer.push_str(&String::from_utf8_lossy(&bytes));
                while let Some(frame_end) = buffer.find("\n\n") {
                    let frame = buffer[..frame_end].to_string();
                    buffer = buffer[frame_end + 2..].to_string();
                    for line in frame.lines() {
                        let Some(data) = line.trim().strip_prefix("data:") else { continue };
                        let data = data.trim();
                        if data.is_empty() || data == "[DONE]" { continue; }
                        if let Ok(v) = serde_json::from_str::<Value>(data) {
                            if let Some(chunk) = Self::parse_chunk(&v, &model, &mut collected_usage) {
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
                    id: format!("chatcmpl-agr-{}", chrono::Utc::now().timestamp()),
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
        let reasoning_content = delta
            .get("reasoning_content")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| {
                delta
                    .get("reasoning")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            });
        let content = delta
            .get("content")
            .and_then(|c| c.as_str())
            .map(|s| s.to_string());
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

        let has_content = content.is_some();
        let has_finish = finish.is_some();
        let has_tool_calls = tool_calls.is_some();
        if !has_content && !has_finish && !has_tool_calls && reasoning_content.is_none() {
            return None;
        }

        Some(ChatCompletionChunk {
            id: format!("chatcmpl-agr-{}", chrono::Utc::now().timestamp()),
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

    /// Non-streaming request via `/v1/responses` (OpenAI Responses API).
    /// Translates to/from chat completions format.
    async fn send_collect_responses(
        &self,
        body: Value,
        cred: &AgrCredential,
    ) -> Result<ChatCompletionResponse, GatewayError> {
        let url = format!("{}/responses", constants::BASE_URL);
        let response = self
            .headers(self.http.post(&url), cred)
            .json(&body)
            .send()
            .await
            .map_err(|e| GatewayError::ProviderError(format!("Agr HTTP: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            return Err(GatewayError::ProviderHttpError {
                status,
                body: text,
                provider: "agr".into(),
                key_id: None,
            });
        }

        let json: Value = response
            .json()
            .await
            .map_err(|e| GatewayError::ProviderError(format!("Agr parse: {}", e)))?;

        // Extract assistant text from output[].content[].text where type == "message"
        let content = json
            .get("output")
            .and_then(|o| o.as_array())
            .and_then(|arr| {
                arr.iter().find_map(|item| {
                    if item.get("type").and_then(|t| t.as_str()) == Some("message") {
                        item.get("content")
                            .and_then(|c| c.as_array())
                            .and_then(|content_arr| {
                                content_arr.iter().find_map(|c_item| {
                                    if c_item.get("type").and_then(|t| t.as_str())
                                        == Some("output_text")
                                    {
                                        c_item.get("text").and_then(|t| t.as_str()).map(String::from)
                                    } else {
                                        None
                                    }
                                })
                            })
                    } else {
                        None
                    }
                })
            });

        // Map usage: input_tokens → prompt_tokens, output_tokens → completion_tokens
        let usage = json.get("usage").map(|u| Usage {
            prompt_tokens: u
                .get("input_tokens")
                .and_then(|n| n.as_u64())
                .unwrap_or(0) as u32,
            completion_tokens: u
                .get("output_tokens")
                .and_then(|n| n.as_u64())
                .unwrap_or(0) as u32,
            total_tokens: u
                .get("total_tokens")
                .and_then(|n| n.as_u64())
                .unwrap_or(0) as u32,
        });

        Ok(ChatCompletionResponse {
            id: json
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("agr-unknown")
                .to_string(),
            object: "chat.completion".to_string(),
            created: chrono::Utc::now().timestamp() as u64,
            model: json
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("agr")
                .to_string(),
            choices: vec![Choice {
                index: 0,
                message: Message {
                    role: "assistant".to_string(),
                    content,
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                    reasoning_content: None,
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: usage.or(Some(Usage {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
            })),
        })
    }

    /// Streaming request via `/v1/responses` (OpenAI Responses API).
    /// Parses SSE events and translates to chat completion chunks.
    async fn send_stream_responses(
        &self,
        body: Value,
        cred: &AgrCredential,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, GatewayError>>, GatewayError> {
        let url = format!("{}/responses", constants::BASE_URL);
        let response = self
            .headers(self.http.post(&url), cred)
            .json(&body)
            .send()
            .await
            .map_err(|e| GatewayError::ProviderError(format!("Agr HTTP: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            return Err(GatewayError::ProviderHttpError {
                status,
                body: text,
                provider: "agr".into(),
                key_id: None,
            });
        }

        let model = body
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("agr")
            .to_string();
        let upstream = response.bytes_stream();

        let parsed = async_stream::try_stream! {
            let mut buffer = String::new();
            let mut collected_usage: Option<Usage> = None;
            let first_chunk_timeout = std::time::Duration::from_secs(constants::STREAM_FIRST_CHUNK_TIMEOUT_SECS);
            let stall_timeout = std::time::Duration::from_secs(constants::STREAM_STALL_TIMEOUT_SECS);
            let mut first = true;
            futures::pin_mut!(upstream);
            loop {
                let wait = if first { first_chunk_timeout } else { stall_timeout };
                first = false;
                let next = tokio::time::timeout(wait, upstream.next()).await
                    .map_err(|_| GatewayError::ProviderError(format!("Agr stream timeout: {}s", wait.as_secs())))?;
                let Some(maybe_bytes) = next else { break };
                let bytes = maybe_bytes.map_err(|e| GatewayError::ProviderError(format!("Agr stream read: {}", e)))?;
                buffer.push_str(&String::from_utf8_lossy(&bytes));
                // Responses API SSE format: "event: <type>\ndata: {json}\n\n"
                while let Some(frame_end) = buffer.find("\n\n") {
                    let frame = buffer[..frame_end].to_string();
                    buffer = buffer[frame_end + 2..].to_string();
                    let mut event_type: Option<String> = None;
                    let mut data_str: Option<String> = None;
                    for line in frame.lines() {
                        let line = line.trim();
                        if let Some(ev) = line.strip_prefix("event:") {
                            event_type = Some(ev.trim().to_string());
                        } else if let Some(data) = line.strip_prefix("data:") {
                            data_str = Some(data.trim().to_string());
                        }
                    }
                    let Some(data_str) = data_str else { continue };
                    if data_str.is_empty() || data_str == "[DONE]" { continue; }
                    let Ok(v) = serde_json::from_str::<Value>(&data_str) else { continue };
                    let ev_type = event_type.as_deref().or_else(|| {
                        v.get("type").and_then(|t| t.as_str())
                    });

                    match ev_type {
                        Some("response.output_text.delta") => {
                            if let Some(delta_text) = v.get("delta").and_then(|d| d.as_str()) {
                                yield ChatCompletionChunk {
                                    id: format!("chatcmpl-agr-{}", chrono::Utc::now().timestamp()),
                                    object: "chat.completion.chunk".to_string(),
                                    created: chrono::Utc::now().timestamp() as u64,
                                    model: model.to_string(),
                                    choices: vec![ChunkChoice {
                                        index: 0,
                                        delta: Delta {
                                            role: None,
                                            content: Some(delta_text.to_string()),
                                            reasoning_content: None,
                                            tool_calls: None,
                                        },
                                        finish_reason: None,
                                    }],
                                    usage: None,
                                };
                            }
                        }
                        Some("response.completed") => {
                            // Extract usage from the full response object
                            if let Some(resp) = v.get("response") {
                                if let Some(u) = resp.get("usage") {
                                    collected_usage = Some(Usage {
                                        prompt_tokens: u.get("input_tokens").and_then(|n| n.as_u64()).unwrap_or(0) as u32,
                                        completion_tokens: u.get("output_tokens").and_then(|n| n.as_u64()).unwrap_or(0) as u32,
                                        total_tokens: u.get("total_tokens").and_then(|n| n.as_u64()).unwrap_or(0) as u32,
                                    });
                                }
                            }
                            // Emit final chunk with finish_reason + usage
                            let final_usage = collected_usage.take();
                            yield ChatCompletionChunk {
                                id: format!("chatcmpl-agr-{}", chrono::Utc::now().timestamp()),
                                object: "chat.completion.chunk".to_string(),
                                created: chrono::Utc::now().timestamp() as u64,
                                model: model.to_string(),
                                choices: vec![ChunkChoice {
                                    index: 0,
                                    delta: Delta {
                                        role: None,
                                        content: None,
                                        reasoning_content: None,
                                        tool_calls: None,
                                    },
                                    finish_reason: Some("stop".to_string()),
                                }],
                                usage: final_usage,
                            };
                        }
                        _ => {
                            // Ignore: response.created, response.in_progress,
                            // response.output_item.added/done, reasoning events, etc.
                        }
                    }
                }
            }
        };
        Ok(parsed.boxed())
    }
}

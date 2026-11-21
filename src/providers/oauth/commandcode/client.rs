use crate::error::GatewayError;
use crate::types::chat::{
    ChatCompletionChunk, ChatCompletionResponse, ChunkChoice, ChunkToolCall,
    ChunkToolCallFunction, Delta, Message, ToolCall, ToolCallFunction, Usage,
};
use futures::stream::{BoxStream, StreamExt};
use serde_json::{json, Value};
use std::io::Read;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::auth::CcOAuthCredential;
use super::constants;
use super::mapper::CcMapper;

/// Path to the Python transport script.
const TRANSPORT_SCRIPT: &str =
    "src/providers/oauth/commandcode/transport.py";

pub struct CcClient {
    first_chunk_timeout: std::time::Duration,
    stall_timeout: std::time::Duration,
}

impl CcClient {
    pub fn new() -> Self {
        Self {
            first_chunk_timeout: std::time::Duration::from_secs(
                constants::STREAM_FIRST_CHUNK_TIMEOUT_SECS,
            ),
            stall_timeout: std::time::Duration::from_secs(constants::STREAM_STALL_TIMEOUT_SECS),
        }
    }

    /// Build the JSON payload for the Python transport script.
    fn build_transport_input(
        body: &Value,
        cred: &CcOAuthCredential,
    ) -> Value {
        let session_id = CcMapper::session_id(body);
        let session_id = if session_id.is_empty() {
            uuid::Uuid::new_v4().to_string()
        } else {
            session_id
        };

        // Clone body without the internal _sessionId field
        let mut wire_body = body.clone();
        if let Some(obj) = wire_body.as_object_mut() {
            obj.remove("_sessionId");
        }

        json!({
            "url": constants::CHAT_URL,
            "token": cred.access_token,
            "body": wire_body,
            "session_id": session_id,
        })
    }

    /// Non-streaming: run the stream to completion and aggregate
    /// (same pattern as Codex — Command Code always streams via NDJSON).
    pub async fn send_collect(
        &self,
        body: Value,
        cred: &CcOAuthCredential,
    ) -> Result<ChatCompletionResponse, GatewayError> {
        if cred.access_token.is_empty() {
            return Err(GatewayError::ProviderError(
                "CommandCode: access_token missing".into(),
            ));
        }

        let mut stream = self.send_stream(body, cred).await?;
        let mut out = String::new();
        let mut reasoning_buf = String::new();
        let mut usage: Option<Usage> = None;
        let mut finish_reason: Option<String> = None;
        let mut tool_calls: std::collections::BTreeMap<u32, (String, String, String)> =
            std::collections::BTreeMap::new();

        while let Some(item) = stream.next().await {
            let chunk = item?;
            for choice in chunk.choices {
                if let Some(c) = choice.delta.content {
                    out.push_str(&c);
                }
                if let Some(r) = choice.delta.reasoning_content {
                    reasoning_buf.push_str(&r);
                }
                if let Some(fr) = choice.finish_reason {
                    finish_reason = Some(fr);
                }
                if let Some(tcs) = choice.delta.tool_calls {
                    for tc in tcs {
                        let entry = tool_calls
                            .entry(tc.index)
                            .or_insert_with(|| (String::new(), String::new(), String::new()));
                        if let Some(id) = tc.id {
                            entry.0 = id;
                        }
                        if let Some(func) = tc.function {
                            if let Some(name) = func.name {
                                entry.1 = name;
                            }
                            if let Some(args) = func.arguments {
                                entry.2.push_str(&args);
                            }
                        }
                    }
                }
            }
            if chunk.usage.is_some() {
                usage = chunk.usage;
            }
        }

        let reasoning_content = if reasoning_buf.is_empty() {
            None
        } else {
            Some(reasoning_buf)
        };
        let tool_calls_vec: Vec<ToolCall> = tool_calls
            .into_iter()
            .filter(|(_, (id, name, _))| !id.is_empty() && !name.is_empty())
            .map(|(_, (id, name, arguments))| ToolCall {
                id,
                type_: "function".to_string(),
                function: ToolCallFunction { name, arguments },
            })
            .collect();
        let tool_calls_opt = if tool_calls_vec.is_empty() {
            None
        } else {
            Some(tool_calls_vec)
        };

        Ok(ChatCompletionResponse {
            id: format!("chatcmpl-cc-{}", chrono::Utc::now().timestamp()),
            object: "chat.completion".to_string(),
            created: chrono::Utc::now().timestamp() as u64,
            model: "cc".to_string(),
            choices: vec![crate::types::chat::Choice {
                index: 0,
                message: Message {
                    role: "assistant".to_string(),
                    content: if out.is_empty() { None } else { Some(out) },
                    reasoning_content,
                    tool_calls: tool_calls_opt,
                    tool_call_id: None,
                    name: None,
                },
                finish_reason,
            }],
            usage,
        })
    }

    /// Streaming POST. Returns a stream of ChatCompletionChunks.
    /// Command Code uses NDJSON (one JSON per line), NOT SSE.
    /// Uses Python subprocess transport to avoid TLS fingerprint detection.
    pub async fn send_stream(
        &self,
        body: Value,
        cred: &CcOAuthCredential,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, GatewayError>>, GatewayError>
    {
        if cred.access_token.is_empty() {
            return Err(GatewayError::ProviderError(
                "CommandCode: access_token missing".into(),
            ));
        }

        let transport_input = Self::build_transport_input(&body, cred);
        let input_str = serde_json::to_string(&transport_input)
            .map_err(|e| GatewayError::ProviderError(format!("CommandCode serialize: {e}")))?;

        // Spawn python3 transport script
        let mut child = tokio::process::Command::new("python3")
            .arg(TRANSPORT_SCRIPT)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .current_dir("/media/usb-hdd/.hermes/projects/nuvcode/backend")
            .spawn()
            .map_err(|e| GatewayError::ProviderError(format!("CommandCode spawn python3: {e}")))?;

        // Write input to stdin
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(input_str.as_bytes()).await
                .map_err(|e| GatewayError::ProviderError(format!("CommandCode stdin: {e}")))?;
            // stdin dropped here → closes pipe
        }

        // Take stdout and stderr
        let mut stdout = child.stdout.take()
            .ok_or_else(|| GatewayError::ProviderError("CommandCode: no stdout".into()))?;
        let mut stderr = child.stderr.take()
            .ok_or_else(|| GatewayError::ProviderError("CommandCode: no stderr".into()))?;

        // Read first bytes with timeout to detect immediate errors
        let mut first_buf = vec![0u8; 1];
        let first_read = tokio::time::timeout(
            std::time::Duration::from_secs(constants::DEFAULT_TIMEOUT_SECS),
            stdout.read(&mut first_buf),
        ).await
            .map_err(|_| GatewayError::ProviderError("CommandCode: timeout waiting for first byte".into()))?
            .map_err(|e| GatewayError::ProviderError(format!("CommandCode read: {e}")))?;

        if first_read == 0 {
            // No stdout output — check stderr for error
            let mut err_buf = String::new();
            let _ = stderr.read_to_string(&mut err_buf).await;
            let err_msg = if err_buf.is_empty() {
                "CommandCode: empty response from transport".to_string()
            } else {
                // Try parse JSON error
                if let Ok(err_data) = serde_json::from_str::<Value>(&err_buf) {
                    let status = err_data.get("status").and_then(|s| s.as_u64()).unwrap_or(502) as u16;
                    let msg = err_data.get("error").and_then(|e| e.as_str()).unwrap_or("Unknown error");
                    return Err(GatewayError::ProviderHttpError {
                        status,
                        body: msg.to_string(),
                        provider: constants::PROVIDER_ID.to_string(),
                        key_id: None,
                    });
                }
                err_buf
            };
            return Err(GatewayError::ProviderHttpError {
                status: 502,
                body: err_msg,
                provider: constants::PROVIDER_ID.to_string(),
                key_id: None,
            });
        }

        let model = body
            .get("params")
            .and_then(|p| p.get("model"))
            .and_then(|v| v.as_str())
            .unwrap_or("cc")
            .to_string();

        // Create async stream from stdout
        let ft = self.first_chunk_timeout;
        let st = self.stall_timeout;

        let stream = async_stream::try_stream! {
            // Prepend the first byte we already read
            let mut buf = String::from_utf8_lossy(&first_buf).to_string();

            // Read rest of stdout
            let mut tmp = vec![0u8; 4096];
            let mut first = true;
            loop {
                let wait = if first { ft } else { st };
                first = false;
                let n = tokio::time::timeout(wait, stdout.read(&mut tmp)).await
                    .map_err(|_| GatewayError::ProviderError(
                        format!("CommandCode timeout: {}s", wait.as_secs())
                    ))?
                    .map_err(|e| GatewayError::ProviderError(format!("CommandCode read: {e}")))?;
                if n == 0 { break; }
                buf.push_str(&String::from_utf8_lossy(&tmp[..n]));

                // NDJSON: split by newlines, parse each non-empty line as JSON
                while let Some(nl) = buf.find('\n') {
                    let line = buf[..nl].trim().to_string();
                    buf = buf[nl + 1..].to_string();
                    if line.is_empty() { continue; }

                    let event: Value = match serde_json::from_str(&line) {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::warn!("CommandCode NDJSON parse: {e} — line: {}", &line[..line.len().min(200)]);
                            continue;
                        }
                    };

                    let event_type = event.get("type").and_then(|t| t.as_str()).unwrap_or("");

                    match event_type {
                        "text-delta" => {
                            let text = event.get("text").and_then(|t| t.as_str()).unwrap_or("");
                            yield Self::make_chunk(&model, Some(text.to_string()), None, None, None);
                        }
                        "reasoning-delta" => {
                            let text = event.get("text").and_then(|t| t.as_str()).unwrap_or("");
                            yield Self::make_chunk(&model, None, Some(text.to_string()), None, None);
                        }
                        "tool-call" => {
                            let tool_call_id = event.get("toolCallId").and_then(|t| t.as_str()).unwrap_or("").to_string();
                            let tool_name = event.get("toolName").and_then(|t| t.as_str()).unwrap_or("").to_string();
                            let input = event.get("input").cloned().unwrap_or(json!({}));
                            let input_str = serde_json::to_string(&input).unwrap_or_default();
                            yield Self::make_tool_chunk(&model, 0, &tool_call_id, &tool_name, &input_str);
                        }
                        "finish" => {
                            let fr = event.get("finishReason").and_then(|t| t.as_str()).unwrap_or("stop");
                            let mapped = Self::map_finish_reason(fr);
                            let usage = Self::extract_usage(&event);
                            yield Self::make_finish_chunk(&model, &mapped, usage);
                        }
                        "finish-step" => {
                            if let Some(usage) = Self::extract_usage(&event) {
                                yield Self::make_usage_chunk(&model, usage);
                            }
                        }
                        "error" => {
                            let status_code = event.get("statusCode").and_then(|s| s.as_u64()).unwrap_or(502) as u16;
                            let error_msg = event.get("error").and_then(|e| e.as_str()).unwrap_or("Unknown error");
                            Err(GatewayError::ProviderHttpError {
                                status: status_code,
                                body: error_msg.to_string(),
                                provider: constants::PROVIDER_ID.to_string(),
                                key_id: None,
                            })?;
                        }
                        "abort" => {
                            Err(GatewayError::ProviderHttpError {
                                status: 502,
                                body: "CommandCode stream aborted".to_string(),
                                provider: constants::PROVIDER_ID.to_string(),
                                key_id: None,
                            })?;
                        }
                        "cache-write-tokens" => {}
                        _ => {
                            tracing::debug!("CommandCode unknown event: {event_type}");
                        }
                    }
                }
            }
        };

        Ok(stream.boxed())
    }

    fn make_chunk(
        model: &str,
        content: Option<String>,
        reasoning: Option<String>,
        tool_calls: Option<Vec<ChunkToolCall>>,
        finish_reason: Option<String>,
    ) -> ChatCompletionChunk {
        ChatCompletionChunk {
            id: format!("chatcmpl-cc-{}", chrono::Utc::now().timestamp()),
            object: "chat.completion.chunk".to_string(),
            created: chrono::Utc::now().timestamp() as u64,
            model: model.to_string(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: Delta {
                    role: None,
                    content,
                    reasoning_content: reasoning,
                    tool_calls,
                },
                finish_reason,
            }],
            usage: None,
        }
    }

    fn make_tool_chunk(
        model: &str,
        index: u32,
        id: &str,
        name: &str,
        arguments: &str,
    ) -> ChatCompletionChunk {
        ChatCompletionChunk {
            id: format!("chatcmpl-cc-{}", chrono::Utc::now().timestamp()),
            object: "chat.completion.chunk".to_string(),
            created: chrono::Utc::now().timestamp() as u64,
            model: model.to_string(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: Delta {
                    role: None,
                    content: None,
                    reasoning_content: None,
                    tool_calls: Some(vec![ChunkToolCall {
                        index,
                        id: Some(id.to_string()),
                        type_: Some("function".to_string()),
                        function: Some(ChunkToolCallFunction {
                            name: Some(name.to_string()),
                            arguments: Some(arguments.to_string()),
                        }),
                    }]),
                },
                finish_reason: None,
            }],
            usage: None,
        }
    }

    fn make_finish_chunk(
        model: &str,
        finish_reason: &str,
        usage: Option<Usage>,
    ) -> ChatCompletionChunk {
        ChatCompletionChunk {
            id: format!("chatcmpl-cc-{}", chrono::Utc::now().timestamp()),
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
                finish_reason: Some(finish_reason.to_string()),
            }],
            usage,
        }
    }

    fn make_usage_chunk(model: &str, usage: Usage) -> ChatCompletionChunk {
        ChatCompletionChunk {
            id: format!("chatcmpl-cc-{}", chrono::Utc::now().timestamp()),
            object: "chat.completion.chunk".to_string(),
            created: chrono::Utc::now().timestamp() as u64,
            model: model.to_string(),
            choices: vec![],
            usage: Some(usage),
        }
    }

    fn map_finish_reason(fr: &str) -> String {
        match fr {
            "stop" | "end_turn" => "stop".to_string(),
            "tool-calls" | "tool_use" => "tool_calls".to_string(),
            "max_tokens" => "length".to_string(),
            "refusal" | "content_filter" => "content_filter".to_string(),
            _ => "stop".to_string(),
        }
    }

    fn extract_usage(event: &Value) -> Option<Usage> {
        event
            .get("totalUsage")
            .or_else(|| event.get("usage"))
            .and_then(|u| {
                let input = u.get("inputTokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                let output = u.get("outputTokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                if input == 0 && output == 0 {
                    None
                } else {
                    Some(Usage {
                        prompt_tokens: input,
                        completion_tokens: output,
                        total_tokens: input + output,
                    })
                }
            })
    }
}

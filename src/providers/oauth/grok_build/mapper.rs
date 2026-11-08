use serde_json::{json, Value};

use crate::types::chat::ChatCompletionRequest;

use super::constants;

/// Translate OpenAI Chat Completions request → OpenAI Responses API request.
///
/// Differences:
/// - `messages` becomes `input` (we flatten to a single string for now —
///   Responses API also accepts a structured `messages` array but the
///   flat-string form works for plain text chat and matches the wire
///   capture from the official Grok CLI).
/// - `reasoning.effort` is added (default `high`) — Grok Build requires it.
/// - `stream: true` is forced (Responses API at cli-chat-proxy requires it).
/// - Model id is mapped from `gb/<frontend>` → `<backend>` from MODELS table.
pub struct GbMapper;

impl GbMapper {
    pub fn to_responses_request(&self, request: ChatCompletionRequest) -> Value {
        let model_id = request.model.trim_start_matches("gb/");
        let backend_model = constants::MODELS
            .iter()
            .find(|m| m.id == model_id)
            .map(|m| m.backend_model)
            .unwrap_or(model_id);

        // Decide input shape:
        //   - structured array when history contains tool_calls or
        //     tool results (Hermes Agent multi-turn loops). Preserves
        //     call_id linkage the proxy needs to resolve function_call_output.
        //   - flat string when history is plain user/assistant text —
        //     matches the official CLI wire capture, simplest case.
        let input = build_responses_input(&request.messages);

        // `include: ["reasoning.encrypted_content"]` triggers the proxy
        // to emit a `response.output_item.added` event with the reasoning
        // item (encrypted). Without it the proxy omits reasoning events
        // entirely and we lose the `reasoning_tokens` accounting.
        // `summary: "detailed"` requests the proxy to also emit per-step
        // reasoning summary text deltas when reasoning effort is non-trivial.
        let mut body = json!({
            "model": backend_model,
            "input": input,
            "stream": true,
            "store": false,
            "reasoning": {
                "effort": constants::DEFAULT_REASONING_EFFORT,
                "summary": constants::DEFAULT_REASONING_SUMMARY,
            },
            "include": ["reasoning.encrypted_content"],
            "parallel_tool_calls": true,
        });

        if let Some(temp) = request.temperature {
            body["temperature"] = json!(temp);
        }
        if let Some(max_tokens) = request.max_tokens {
            body["max_output_tokens"] = json!(max_tokens);
        } else if let Some(m) = constants::MODELS.iter().find(|m| m.id == model_id) {
            // No client cap — default to the model's ceiling so long
            // reasoning turns don't hit a small proxy default and get
            // cut off mid-response ("Response truncated due to output
            // length limit").
            if let Some(mt) = m.max_tokens {
                body["max_output_tokens"] = json!(mt);
            }
        }
        if let Some(top_p) = request.top_p {
            body["top_p"] = json!(top_p);
        }
        // Forward tools (function calling) so the model can decide to
        // call them. Without this Grok Build never emits tool_calls
        // chunks and just produces a plain text answer.
        //
        // Chat Completions shape (what FE sends):
        //   `[{type:"function", function:{name, description, parameters}}]`
        // Grok Build Responses API shape (what upstream wants):
        //   `[{type:"function", name, description, parameters}]` (flat)
        //
        // Without this reshape the upstream returns 422:
        //   "missing field `name`"
        if let Some(tools) = request.tools {
            let flat: Vec<Value> = tools
                .iter()
                .map(|t| {
                    json!({
                        "type": t.type_,
                        "name": t.function.name,
                        "description": t.function.description.clone().unwrap_or_default(),
                        "parameters": t.function.parameters.clone().unwrap_or(json!({
                            "type": "object",
                            "properties": {}
                        })),
                    })
                })
                .collect();
            if !flat.is_empty() {
                body["tools"] = Value::Array(flat);
            }
        }
        if let Some(tc) = request.tool_choice {
            body["tool_choice"] = tc;
        }

        body
    }
}

/// Build Responses API `input` from Chat Completions `messages`.
///
/// Auto-detects the right shape:
///   - If any message has `tool_calls` (assistant) or `tool_call_id`
///     (tool role) → return a structured array preserving the
///     function_call / function_call_output items so Grok Build can
///     resolve the call_id linkage across turns.
///   - Otherwise → return a flat string ("role: content" lines) which
///     matches the official CLI wire capture for plain chat.
///
/// Without structured input the model receives `"assistant: "` and
/// `"tool: "` text blobs and loses the call_id binding — Grok Build
/// silently truncates reasoning and the response comes back as
/// "Response truncated due to output length limit".
fn build_responses_input(messages: &[crate::types::chat::Message]) -> Value {
    let has_tools = messages.iter().any(|m| {
        m.tool_calls.is_some() || m.tool_call_id.is_some()
    });
    if !has_tools {
        // Plain text chat — keep the simple flat string form.
        return Value::String(flatten_messages(messages));
    }

    // Multi-turn with tool calls — build structured Responses input array.
    let mut items: Vec<Value> = Vec::with_capacity(messages.len());
    for m in messages {
        match m.role.as_str() {
            "tool" => {
                // Tool result — must reference a prior function_call.
                if let Some(call_id) = m.tool_call_id.as_ref() {
                    items.push(json!({
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": m.content.clone().unwrap_or_default(),
                    }));
                }
                // Tool results without call_id are dropped — proxy
                // rejects them anyway, and Hermes Agent never sends
                // them this way.
            }
            "assistant" => {
                // Assistant message with tool_calls — emit each
                // function_call as a separate item. Plain text content
                // becomes a message item alongside.
                if let Some(tcs) = &m.tool_calls {
                    if let Some(content) = &m.content {
                        if !content.is_empty() {
                            items.push(json!({
                                "type": "message",
                                "role": "assistant",
                                "content": [{
                                    "type": "output_text",
                                    "text": content,
                                }],
                            }));
                        }
                    }
                    for tc in tcs {
                        items.push(json!({
                            "type": "function_call",
                            "id": tc.id,
                            "call_id": tc.id,
                            "name": tc.function.name,
                            "arguments": tc.function.arguments,
                        }));
                    }
                } else if let Some(content) = &m.content {
                    // Plain assistant text — no tool calls.
                    items.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{
                            "type": "output_text",
                            "text": content,
                        }],
                    }));
                }
            }
            _ => {
                // user / system / developer — flatten to message item.
                let content = m.content.clone().unwrap_or_default();
                items.push(json!({
                    "type": "message",
                    "role": m.role,
                    "content": [{
                        "type": if m.role == "system" { "input_text" } else { "input_text" },
                        "text": content,
                    }],
                }));
            }
        }
    }

    // Filter out orphan function_call_output (call_id without matching
    // function_call in the same turn-set). Without this Grok Build
    // returns a 422 "call_id not found".
    let known_call_ids: std::collections::HashSet<String> = items
        .iter()
        .filter_map(|it| {
            it.get("type")
                .and_then(|t| t.as_str())
                .filter(|t| *t == "function_call")
                .and_then(|_| it.get("call_id").and_then(|v| v.as_str()))
                .map(String::from)
        })
        .collect();
    items.retain(|it| {
        if it.get("type").and_then(|t| t.as_str()) == Some("function_call_output") {
            it.get("call_id")
                .and_then(|v| v.as_str())
                .map(|id| known_call_ids.contains(id))
                .unwrap_or(false)
        } else {
            true
        }
    });

    Value::Array(items)
}

/// Flatten OpenAI messages into a single string for the Responses API `input`.
///
/// Used as the simple path when history has no tool calls — matches the
/// official Grok CLI wire capture for plain chat.
fn flatten_messages(messages: &[crate::types::chat::Message]) -> String {
    let mut out = String::new();
    for m in messages {
        let content = m.content.clone().unwrap_or_default();
        if content.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("{}: {}", m.role, content));
    }
    out
}

/// Stateful parser for Responses API SSE events → Chat Completions chunks.
///
/// Holds per-output_index tool call state so the `function_call_arguments.delta`
/// events can be mapped to the right `tool_call_id` for the FE.
/// Multiple parallel tool calls are supported (`output_index` is the key).
pub struct ResponsesStreamParser {
    /// Per-output_index tool call metadata captured from `output_item.added`.
    tool_state: std::collections::HashMap<u64, ToolCallState>,
    /// True once we've emitted a `finish_reason: "tool_calls"` chunk in
    /// this turn. Prevents a duplicate `finish_reason: "stop"` from
    /// arriving on the same turn — OpenAI Chat Completions convention
    /// allows only ONE finish_reason per chunk (last write wins) so a
    /// duplicate would race and confuse the FE aggregator.
    has_tool_call_finish: bool,
}

#[derive(Debug, Clone, Default)]
struct ToolCallState {
    /// Stable id from Responses API (e.g. "fc_xxx") — used as `tool_call.id`
    /// in Chat Completions chunks.
    id: String,
    /// Function name (e.g. "terminal").
    name: String,
    /// 1-based chunk index within this turn's tool_calls array. Matches
    /// OpenAI Chat Completions streaming convention.
    chunk_index: u32,
    /// True once argument content has been emitted from delta events.
    /// Responses API `.done` repeats the full arguments; never forward it
    /// after deltas or downstream clients concatenate JSON.
    arguments_started: bool,
}

impl Default for ResponsesStreamParser {
    fn default() -> Self {
        Self {
            tool_state: std::collections::HashMap::new(),
            has_tool_call_finish: false,
        }
    }
}

impl ResponsesStreamParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Process one Responses API SSE event. Returns 0..N Chat Completions
    /// chunks (some events emit zero — e.g. status updates).
    pub fn process_event(&mut self, event: &Value) -> Vec<Value> {
        let event_type = event.get("type").and_then(|v| v.as_str());
        let ts = now_secs();
        let mut out: Vec<Value> = Vec::new();

        match event_type {
            // Visible output text — goes to the user.
            Some("response.output_text.delta") => {
                if let Some(delta) = event.get("delta").and_then(|v| v.as_str()) {
                    out.push(json!({
                        "id": event.get("response_id").and_then(|v| v.as_str()).unwrap_or("gb"),
                        "object": "chat.completion.chunk",
                        "created": ts,
                        "model": "grok-4.5",
                        "choices": [{
                            "index": 0,
                            "delta": {"content": delta},
                            "finish_reason": null,
                        }],
                    }));
                }
            }
            // Per-step reasoning summary text. Sent as `reasoning_content`
            // (deepseek/glm convention — NuvCode already supports it).
            Some("response.reasoning_summary_text.delta") => {
                if let Some(delta) = event.get("delta").and_then(|v| v.as_str()) {
                    if !delta.is_empty() {
                        out.push(json!({
                            "id": "gb",
                            "object": "chat.completion.chunk",
                            "created": ts,
                            "model": "grok-4.5",
                            "choices": [{
                                "index": 0,
                                "delta": {"reasoning_content": delta},
                                "finish_reason": null,
                            }],
                        }));
                    }
                }
            }
            // Reasoning item started — marker chunk so the dispatcher sees
            // something happen while reasoning is in progress.
            Some("response.output_item.added") => {
                if let Some(item) = event.get("item") {
                    let item_type = item.get("type").and_then(|v| v.as_str());
                    match item_type {
                        Some("reasoning") => {
                            out.push(json!({
                                "id": "gb",
                                "object": "chat.completion.chunk",
                                "created": ts,
                                "model": "grok-4.5",
                                "choices": [{
                                    "index": 0,
                                    "delta": {"reasoning_content": ""},
                                    "finish_reason": null,
                                }],
                            }));
                        }
                        Some("function_call") | Some("custom_tool_call") => {
                            // Capture tool metadata so subsequent delta events
                            // can attach to the right tool_call chunk.
                            // Grok Build emits `custom_tool_call` (not
                            // `function_call`) — both map to the same
                            // OpenAI `tool_calls[]` shape.
                            let output_index = event
                                .get("output_index")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0);
                            let id = item
                                .get("id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let name = item
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let chunk_index = self.tool_state.len() as u32;
                            self.tool_state.insert(
                                output_index,
                                ToolCallState {
                                    id: id.clone(),
                                    name: name.clone(),
                                    chunk_index,
                                    arguments_started: false,
                                },
                            );
                            // custom_tool_call may carry the complete input
                            // inline (no streaming deltas follow) — capture it
                            // so the open chunk already has real arguments.
                            let inline_args = item
                                .get("arguments")
                                .and_then(|v| v.as_str())
                                .map(String::from)
                                .or_else(|| {
                                    item.get("input").and_then(|v| match v {
                                        Value::String(s) => Some(s.clone()),
                                        Value::Object(_) | Value::Array(_) => {
                                            Some(serde_json::to_string(v).unwrap_or_default())
                                        }
                                        _ => None,
                                    })
                                })
                                .unwrap_or_default();
                            // Emit a chunk that opens the tool_call entry
                            // (with id + name + empty arguments). NuvCode's
                            // dispatcher expects `tool_calls[*].index` to
                            // stay stable for the whole call.
                            out.push(json!({
                                "id": "gb",
                                "object": "chat.completion.chunk",
                                "created": ts,
                                "model": "grok-4.5",
                                "choices": [{
                                    "index": 0,
                                    "delta": {
                                        "tool_calls": [{
                                            "index": chunk_index,
                                            "id": id,
                                            "type": "function",
                                            "function": {"name": name, "arguments": inline_args},
                                        }],
                                    },
                                    "finish_reason": null,
                                }],
                            }));
                        }
                        _ => {}
                    }
                }
            }
            // Argument streaming — chunked delta per character.
            Some("response.function_call_arguments.delta")
            | Some("response.custom_tool_call_input.delta") => {
                let output_index = event
                    .get("output_index")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                // custom_tool_call deltas may arrive as an object (raw input)
                // — serialise it so the arguments string stays valid JSON.
                let delta = event
                    .get("delta")
                    .and_then(|v| match v {
                        Value::String(s) => Some(s.clone()),
                        Value::Object(_) | Value::Array(_) => {
                            Some(serde_json::to_string(v).unwrap_or_default())
                        }
                        _ => None,
                    })
                    .unwrap_or_default();
                if !delta.is_empty() {
                    if let Some(state) = self.tool_state.get_mut(&output_index) {
                        state.arguments_started = true;
                    }
                    if let Some(state) = self.tool_state.get(&output_index).cloned() {
                        out.push(json!({
                            "id": "gb",
                            "object": "chat.completion.chunk",
                            "created": ts,
                            "model": "grok-4.5",
                            "choices": [{
                                "index": 0,
                                "delta": {
                                    "tool_calls": [{
                                        "index": state.chunk_index,
                                        "function": {"arguments": delta},
                                    }],
                                },
                                "finish_reason": null,
                            }],
                        }));
                    }
                }
            }
            // Final argument flush — usually redundant with deltas but
            // some proxies only emit this. Forward it for safety.
            Some("response.function_call_arguments.done")
            | Some("response.custom_tool_call_input.done") => {
                let output_index = event
                    .get("output_index")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                // custom_tool_call carries the complete input object in
                // `input` (not a string `arguments`) — serialise it.
                let arguments = event
                    .get("arguments")
                    .and_then(|v| v.as_str())
                    .map(String::from)
                    .or_else(|| {
                        event.get("input").and_then(|v| match v {
                            Value::String(s) => Some(s.clone()),
                            Value::Object(_) | Value::Array(_) => {
                                Some(serde_json::to_string(v).unwrap_or_default())
                            }
                            _ => None,
                        })
                    })
                    .unwrap_or_default();
                if !arguments.is_empty() {
                    let already_streamed = self
                        .tool_state
                        .get(&output_index)
                        .map(|s| s.arguments_started)
                        .unwrap_or(false);
                    if !already_streamed {
                        if let Some(state) = self.tool_state.get(&output_index).cloned() {
                        out.push(json!({
                            "id": "gb",
                            "object": "chat.completion.chunk",
                            "created": ts,
                            "model": "grok-4.5",
                            "choices": [{
                                "index": 0,
                                "delta": {
                                    "tool_calls": [{
                                        "index": state.chunk_index,
                                        "function": {"arguments": arguments},
                                    }],
                                },
                                "finish_reason": null,
                            }],
                        }));
                        }
                    }
                }
            }
            // Tool call complete — finish_reason becomes "tool_calls" so
            // the dispatcher records the usage row and the client knows
            // to execute the tool.
            Some("response.output_item.done") => {
                let item_type = event
                    .get("item")
                    .and_then(|i| i.get("type"))
                    .and_then(|v| v.as_str());
                if item_type == Some("function_call") || item_type == Some("custom_tool_call") {
                    let output_index = event
                        .get("output_index")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    if let Some(state) = self.tool_state.get(&output_index).cloned() {
                        self.has_tool_call_finish = true;
                        out.push(json!({
                            "id": "gb",
                            "object": "chat.completion.chunk",
                            "created": ts,
                            "model": "grok-4.5",
                            "choices": [{
                                "index": 0,
                                "delta": {},
                                "finish_reason": "tool_calls",
                            }],
                        }));
                    }
                }
            }
            // Terminal — usage chunk. finish_reason depends on whether
            // this turn already emitted a "tool_calls" finish:
            //   - text-only turn → finish_reason = "stop"
            //   - tool-call turn → finish_reason = "tool_calls" (single
            //     finish per turn, OpenAI Chat Completions convention).
            // Dispatcher (`api/chat/streaming.rs`) flushes the usage row
            // whenever `usage.is_some()`, regardless of finish_reason.
            Some("response.completed") | Some("response.incomplete") => {
                let finish = if self.has_tool_call_finish {
                    "tool_calls"
                } else if event_type == Some("response.incomplete") {
                    "length"
                } else {
                    "stop"
                };
                let mut chunk = json!({
                    "id": "gb",
                    "object": "chat.completion.chunk",
                    "created": ts,
                    "model": "grok-4.5",
                    "choices": [{
                        "index": 0,
                        "delta": {},
                        "finish_reason": finish,
                    }],
                });
                if let Some(usage) = event.get("response").and_then(|r| r.get("usage")) {
                    let input = usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                    let output = usage.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                    let total = usage
                        .get("total_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(input + output);
                    let mut usage_obj = json!({
                        "prompt_tokens": input,
                        "completion_tokens": output,
                        "total_tokens": total,
                    });
                    if let Some(reasoning_tokens) = usage
                        .get("output_tokens_details")
                        .and_then(|d| d.get("reasoning_tokens"))
                        .and_then(|v| v.as_u64())
                    {
                        usage_obj["completion_tokens_details"] = json!({
                            "reasoning_tokens": reasoning_tokens,
                        });
                    }
                    chunk["usage"] = usage_obj;
                }
                out.push(chunk);
            }
            _ => {}
        }

        out
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

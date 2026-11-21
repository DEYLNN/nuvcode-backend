use serde_json::{json, Value};

use crate::types::chat::ChatCompletionRequest;

use super::constants;

/// Translate OpenAI Chat Completions request → Command Code custom wire format.
///
/// Key differences from OpenAI:
/// - System prompt is EXTRACTED from messages and put in separate `system` field
/// - Messages use `content` as array of parts: [{type: "text", text: "..."}]
/// - Assistant tool_calls become: {type: "tool-call", toolCallId, toolName, input: <object>}
/// - Tool results become: {role: "tool", content: [{type: "tool-result", toolCallId, toolName, output: {type: "text", value: "..."}}]}
/// - Tools use `input_schema` instead of `parameters`
/// - tool_choice: "none" → omit tools entirely
/// - Wrapped in a top-level envelope with config/params
pub struct CcMapper;

impl CcMapper {
    pub fn to_chat_request(&self, request: ChatCompletionRequest) -> Value {
        let model_id = request.model.trim_start_matches("cc/");
        let backend_model = constants::MODELS
            .iter()
            .find(|m| m.id == model_id)
            .map(|m| m.backend_model)
            .unwrap_or(model_id);

        // Extract system messages → join into `system` field
        let mut system_parts: Vec<String> = Vec::new();
        let mut messages: Vec<Value> = Vec::new();

        // Determine if tools should be omitted (tool_choice: "none")
        let tools_disabled = request
            .tool_choice
            .as_ref()
            .and_then(|v| v.as_str())
            .map(|s| s == "none")
            .unwrap_or(false);

        for msg in &request.messages {
            let role = msg.role.as_str();

            if role == "system" || role == "developer" {
                let content = msg.content.clone().unwrap_or_default();
                if !content.is_empty() {
                    system_parts.push(content);
                }
                continue;
            }

            if role == "assistant" {
                // Assistant message — may have content and/or tool_calls
                let mut msg_obj = json!({
                    "role": "assistant",
                });

                if let Some(content) = &msg.content {
                    if !content.is_empty() {
                        msg_obj["content"] = json!([
                            { "type": "text", "text": content }
                        ]);
                    }
                }

                if let Some(tool_calls) = &msg.tool_calls {
                    let tc_parts: Vec<Value> = tool_calls
                        .iter()
                        .map(|tc| {
                            // arguments is a JSON string → parse to object for wire format
                            let input: Value = serde_json::from_str(&tc.function.arguments)
                                .unwrap_or(json!({}));
                            json!({
                                "type": "tool-call",
                                "toolCallId": tc.id,
                                "toolName": tc.function.name,
                                "input": input,
                            })
                        })
                        .collect();
                    if !tc_parts.is_empty() {
                        // Assistant tool_calls become content parts in CC format
                        let mut content = msg_obj["content"].as_array().cloned().unwrap_or_default();
                        content.extend(tc_parts);
                        msg_obj["content"] = Value::Array(content);
                    }
                }

                messages.push(msg_obj);
                continue;
            }

            if role == "tool" {
                // Tool result message — convert to CC tool-result format
                let tool_call_id = msg.tool_call_id.clone().unwrap_or_default();
                let content = msg.content.clone().unwrap_or_default();
                let tool_name = msg.name.clone().unwrap_or_default();

                messages.push(json!({
                    "role": "tool",
                    "content": [{
                        "type": "tool-result",
                        "toolCallId": tool_call_id,
                        "toolName": tool_name,
                        "output": {
                            "type": "text",
                            "value": content,
                        }
                    }]
                }));
                continue;
            }

            // user (default)
            let content = msg.content.clone().unwrap_or_default();
            messages.push(json!({
                "role": role,
                "content": [{ "type": "text", "text": content }],
            }));
        }

        // Build params
        let mut params = json!({
            "model": backend_model,
            "messages": messages,
            "stream": true,
        });

        if !system_parts.is_empty() {
            params["system"] = json!(system_parts.join("\n\n"));
        }

        if let Some(temp) = request.temperature {
            params["temperature"] = json!(temp);
        }
        if let Some(max_tokens) = request.max_tokens {
            params["max_tokens"] = json!(max_tokens);
        }
        if let Some(top_p) = request.top_p {
            params["top_p"] = json!(top_p);
        }

        // Tools — use input_schema instead of parameters, omit if tool_choice is "none"
        if !tools_disabled {
            if let Some(tools) = &request.tools {
                let mapped_tools: Vec<Value> = tools
                    .iter()
                    .map(|t| {
                        json!({
                            "name": t.function.name,
                            "description": t.function.description.clone().unwrap_or_default(),
                            "input_schema": t.function.parameters.clone().unwrap_or(json!({"type": "object", "properties": {}})),
                        })
                    })
                    .collect();
                if !mapped_tools.is_empty() {
                    params["tools"] = Value::Array(mapped_tools);
                }
            }
        }

        // Build the full envelope
        let thread_id = uuid::Uuid::new_v4().to_string();
        let session_id = uuid::Uuid::new_v4().to_string();

        let mut body = json!({
            "config": {
                "workingDir": "/",
                "isGitRepo": false,
                "currentBranch": "",
                "mainBranch": "",
                "gitStatus": "",
                "recentCommits": [],
                "date": chrono::Utc::now().format("%Y-%m-%d").to_string(),
                "environment": "linux-aarch64",
                "structure": [],
            },
            "memory": null,
            "taste": null,
            "skills": null,
            "permissionMode": "standard",
            "mode": "agent",
            "threadId": thread_id,
            "params": params,
        });

        // session_id is used in the header, but also store for reference
        body["_sessionId"] = json!(session_id);

        body
    }

    /// Extract the session_id from the mapped body (for the x-session-id header).
    pub fn session_id(body: &Value) -> String {
        body.get("_sessionId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    }
}

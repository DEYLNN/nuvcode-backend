use serde_json::{json, Value};

use crate::types::chat::ChatCompletionRequest;

pub struct CbaiMapper;

impl CbaiMapper {
    /// Map our model id (cbai/nvidia-nemotron-...) to CodeBuddy's backend model (nvidia/nemotron-...:free)
    pub fn to_chat_request(&self, request: ChatCompletionRequest) -> Value {
        let model_id = request.model.trim_start_matches("cbai/");
        let backend_model = model_id;
        let reasoning_effort = request.reasoning_effort.clone();
        let mut body = json!({
            "model": backend_model,
            "messages": request.messages,
            "stream": true,
        });
        if let Some(effort) = reasoning_effort {
            if effort != "none" && effort != "off" {
                body["reasoning_effort"] = json!(effort);
                body["reasoning_summary"] = json!("auto");
            }
        }
        let messages = body["messages"].as_array().cloned().unwrap_or_default();
        let mut transformed = vec![json!({"role":"system","content":"You are CodeBuddy Code."})];
        for message in messages {
            if matches!(message.get("role").and_then(Value::as_str), Some("system" | "developer")) { continue; }
            let mut message = message;
            if message.get("role").and_then(Value::as_str) == Some("user") && message["content"].is_string() {
                let text = message["content"].take();
                message["content"] = json!([{"type":"text","text":text}]);
            }
            transformed.push(message);
        }
        body["messages"] = json!(transformed);
        if let Some(temp) = request.temperature {
            body["temperature"] = json!(temp);
        }
        if let Some(max_tokens) = request.max_tokens {
            body["max_tokens"] = json!(max_tokens);
        }
        if let Some(top_p) = request.top_p {
            body["top_p"] = json!(top_p);
        }
        if let Some(tools) = request.tools {
            body["tools"] = json!(tools);
        }
        if let Some(tool_choice) = request.tool_choice {
            body["tool_choice"] = tool_choice;
        }
        // The upstream always streams (forced above), so usage only arrives when we ask
        // for it explicitly. Without include_usage the terminal chunk carries no token
        // counts and every request records 0/0 in the usage table.
        body["stream_options"] = json!({"include_usage": true});
        body
    }
}

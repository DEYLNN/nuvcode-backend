use serde_json::{json, Value};

use crate::types::chat::ChatCompletionRequest;

pub struct AgsMapper;

impl AgsMapper {
    /// Build the OpenAI-compatible request body for Agnes AI.
    pub fn build_body(&self, request: &ChatCompletionRequest, stream: bool) -> Value {
        let model_name = request.model.strip_prefix("ags/").unwrap_or(&request.model);
        let mut body = json!({
            "model": model_name,
            "messages": request.messages.iter().filter_map(|m| serde_json::to_value(m).ok()).collect::<Vec<_>>(),
            "stream": stream,
            "max_tokens": request.max_tokens.unwrap_or(2048),
        });
        if let Some(v) = request.temperature {
            body["temperature"] = json!(v);
        }
        if let Some(v) = request.top_p {
            body["top_p"] = json!(v);
        }
        if let Some(ref v) = request.tools {
            body["tools"] = serde_json::to_value(v).unwrap_or_default();
        }
        if let Some(ref v) = request.tool_choice {
            body["tool_choice"] = v.clone();
        }
        if stream {
            body["stream_options"] = json!({"include_usage": true});
        } else if let Some(ref v) = request.stream_options {
            body["stream_options"] = v.clone();
        }
        body
    }
}

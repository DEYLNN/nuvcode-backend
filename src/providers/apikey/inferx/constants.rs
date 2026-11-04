pub const PROVIDER_ID: &str = "infx";
pub const PROVIDER_NAME: &str = "InferX";
pub const CATEGORY: &str = "apikey";
pub const COLOR: &str = "#6C5CE7";
pub const ICON_NAME: &str = "infx.jpg";
pub const BASE_URL: &str = "https://model.inferx.net/endpoints";
pub const DEFAULT_TIMEOUT_SECS: u64 = 120;
pub const STREAM_FIRST_CHUNK_TIMEOUT_SECS: u64 = 200;
pub const STREAM_STALL_TIMEOUT_SECS: u64 = 360;
pub const USER_AGENT: &str = "nuvcode/1.0";

/// All InferX models are thinking models — reasoning is always hidden
/// downstream: `reasoning_content` field dropped + <think> blocks stripped.
pub const THINKING_TAGS: &[&str] = &["think", "thinking", "reasoning"];

#[derive(Debug, Clone)]
pub struct ModelDef { pub id: &'static str, pub context_length: u32 }

// ponytail: context_length are estimates — correct after first /v1/models probe
pub const MODELS: &[ModelDef] = &[
    ModelDef { id: "deepseek-v4-flash-0731", context_length: 1000000 },
    ModelDef { id: "glm-5.3-flash", context_length: 262144 },
    ModelDef { id: "Qwen3.8-27B-FP8", context_length: 262000 },
];

pub fn provider_spec() -> crate::providers::spec::ProviderSpec {
    crate::providers::spec::ProviderSpec {
        id: PROVIDER_ID,
        name: PROVIDER_NAME,
        full_name: "infx",
        category: CATEGORY,
        base_url: BASE_URL,
        validate_url: "https://model.inferx.net/endpoints/v1/models",
        compatible_api: "openai-chat",
        supports_streaming: true,
        supports_tools: true,
        supports_vision: false,
        color: COLOR,
        icon_name: ICON_NAME,
        usage_url: None,
        quirks: Default::default(),
    }
}

pub const PROVIDER_ID: &str = "verb";
pub const PROVIDER_NAME: &str = "Verboo AI";
pub const CATEGORY: &str = "apikey";
pub const COLOR: &str = "#6366F1";
pub const ICON_NAME: &str = "verb.png";
pub const BASE_URL: &str = "https://code.verboo.ai/router/v1";
pub const DEFAULT_TIMEOUT_SECS: u64 = 64;
pub const STREAM_FIRST_CHUNK_TIMEOUT_SECS: u64 = 120;
pub const STREAM_STALL_TIMEOUT_SECS: u64 = 120;
pub const USER_AGENT: &str = "nuvcode/1.0";

/// All Verboo models are thinking models — reasoning is always hidden
/// downstream: `reasoning_content` field dropped + think-block tags stripped.
pub const THINKING_TAGS: &[&str] = &["think", "thinking", "reasoning"];

#[derive(Debug, Clone)]
pub struct ModelDef {
    pub id: &'static str,
    pub context_length: u32,
}

pub const MODELS: &[ModelDef] = &[
    ModelDef {
        id: "deepseek-v4-flash-0731",
        context_length: 1000000,
    },
];

pub fn provider_spec() -> crate::providers::spec::ProviderSpec {
    crate::providers::spec::ProviderSpec {
        id: PROVIDER_ID,
        name: PROVIDER_NAME,
        full_name: "verboo",
        category: CATEGORY,
        base_url: BASE_URL,
        validate_url: "https://code.verboo.ai/router/v1/models",
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

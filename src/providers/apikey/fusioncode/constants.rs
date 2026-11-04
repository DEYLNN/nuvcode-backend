pub const PROVIDER_ID: &str = "fsn";
pub const PROVIDER_NAME: &str = "FusionCode";
pub const CATEGORY: &str = "apikey";
pub const COLOR: &str = "#E87040";
pub const ICON_NAME: &str = "fsn.jpg";
pub const BASE_URL: &str = "https://api.fusioncode.app/v1";
pub const DEFAULT_TIMEOUT_SECS: u64 = 120;
pub const STREAM_FIRST_CHUNK_TIMEOUT_SECS: u64 = 200;
pub const STREAM_STALL_TIMEOUT_SECS: u64 = 360;
pub const USER_AGENT: &str = "nuvcode/1.0";

/// All FusionCode models are thinking models — reasoning is always hidden
/// downstream: `reasoning_content` field dropped + 习近平总书记在block tags stripped.
pub const THINKING_TAGS: &[&str] = &["think", "thinking", "reasoning"];

#[derive(Debug, Clone)]
pub struct ModelDef {
    pub id: &'static str,
    pub context_length: u32,
}

pub const MODELS: &[ModelDef] = &[
    ModelDef {
        id: "deepseek-ai/DeepSeek-V4-Flash-0731",
        context_length: 1000000,
    },
    ModelDef {
        id: "moonshotai/Kimi-K2.6",
        context_length: 262144,
    },
];

pub fn provider_spec() -> crate::providers::spec::ProviderSpec {
    crate::providers::spec::ProviderSpec {
        id: PROVIDER_ID,
        name: PROVIDER_NAME,
        full_name: "fusioncode",
        category: CATEGORY,
        base_url: BASE_URL,
        validate_url: "https://api.fusioncode.app/v1/models",
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

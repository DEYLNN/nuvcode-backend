pub const PROVIDER_ID: &str = "or";
pub const PROVIDER_NAME: &str = "OpenRouter";
pub const CATEGORY: &str = "apikey";
pub const COLOR: &str = "#F97316";
pub const ICON_NAME: &str = "openrouter.png";
pub const BASE_URL: &str = "https://openrouter.ai/api";
pub const DEFAULT_TIMEOUT_SECS: u64 = 120;
pub const STREAM_FIRST_CHUNK_TIMEOUT_SECS: u64 = 200;
pub const STREAM_STALL_TIMEOUT_SECS: u64 = 360;
pub const USER_AGENT: &str = "nuvcode/1.0";

#[derive(Debug, Clone)]
pub struct ModelDef {
    pub id: &'static str,
    pub context_length: u32,
}

pub const MODELS: &[ModelDef] = &[
    ModelDef {
        id: "baidu/ernie-5.5-flash-preview:free",
        context_length: 1000000,
    },
];

pub fn provider_spec() -> crate::providers::spec::ProviderSpec {
    crate::providers::spec::ProviderSpec {
        id: PROVIDER_ID,
        name: PROVIDER_NAME,
        full_name: "openrouter",
        category: CATEGORY,
        base_url: BASE_URL,
        validate_url: "https://openrouter.ai/api/v1/models",
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

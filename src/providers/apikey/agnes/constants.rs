pub const PROVIDER_ID: &str = "ags";
pub const PROVIDER_NAME: &str = "Agnes AI";
pub const CATEGORY: &str = "apikey";
pub const COLOR: &str = "#8B5CF6";
pub const ICON_NAME: &str = "ags.png";
pub const BASE_URL: &str = "https://apihub.agnes-ai.com";
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
        id: "agnes-2.0-flash",
        context_length: 500000,
    },
    ModelDef {
        id: "agnes-2.5-flash",
        context_length: 500000,
    },
];

pub fn provider_spec() -> crate::providers::spec::ProviderSpec {
    crate::providers::spec::ProviderSpec {
        id: PROVIDER_ID,
        name: PROVIDER_NAME,
        full_name: "agnes",
        category: CATEGORY,
        base_url: BASE_URL,
        validate_url: "https://apihub.agnes-ai.com/v1/models",
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

pub const PROVIDER_ID: &str = "nibie";
pub const PROVIDER_NAME: &str = "Nibie App";
pub const CATEGORY: &str = "apikey";
pub const COLOR: &str = "#10B981";
pub const ICON_NAME: &str = "nibie.svg";
pub const BASE_URL: &str = "https://router.nibie.app/v1";
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
        id: "deepseek-v4.1-flash-free",
        context_length: 1000000,
    },
];

pub fn provider_spec() -> crate::providers::spec::ProviderSpec {
    crate::providers::spec::ProviderSpec {
        id: PROVIDER_ID,
        name: PROVIDER_NAME,
        full_name: "nibie",
        category: CATEGORY,
        base_url: BASE_URL,
        validate_url: "https://router.nibie.app/v1/models",
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

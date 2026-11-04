// Unsloth provider constants — fill these in when ready.
// This is a manual provider (custom logic, not TOML/apikey/oauth).

pub const PROVIDER_ID: &str = "uns";
pub const PROVIDER_NAME: &str = "Unsloth";
pub const CATEGORY: &str = "manual";
pub const COLOR: &str = "#6366F1";
pub const ICON_NAME: &str = "unsloth.png";
pub const BASE_URL: &str = ""; // TODO: fill when ready
pub const DEFAULT_TIMEOUT_SECS: u64 = 120;
pub const STREAM_FIRST_CHUNK_TIMEOUT_SECS: u64 = 200;
pub const STREAM_STALL_TIMEOUT_SECS: u64 = 360;
pub const USER_AGENT: &str = "nuvcode/1.0";

#[derive(Debug, Clone)]
pub struct ModelDef {
    pub id: &'static str,
    pub context_length: u32,
}

// TODO: fill models when ready
pub const MODELS: &[ModelDef] = &[];

pub fn provider_spec() -> crate::providers::spec::ProviderSpec {
    crate::providers::spec::ProviderSpec {
        id: PROVIDER_ID,
        name: PROVIDER_NAME,
        full_name: "unsloth",
        category: CATEGORY,
        base_url: BASE_URL,
        validate_url: "", // TODO
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

pub const PROVIDER_ID: &str = "cf";
pub const PROVIDER_NAME: &str = "Cloudflare";
pub const CATEGORY: &str = "apikey";
pub const COLOR: &str = "#F38020";
pub const ICON_NAME: &str = "cf.png";
pub const BASE_URL: &str = "https://api.cloudflare.com/client/v4/accounts";
pub const DEFAULT_TIMEOUT_SECS: u64 = 90;
pub const STREAM_FIRST_CHUNK_TIMEOUT_SECS: u64 = 60;
pub const STREAM_STALL_TIMEOUT_SECS: u64 = 120;
pub const USER_AGENT: &str = "nuvcode/1.0";

pub fn provider_spec() -> crate::providers::spec::ProviderSpec {
    crate::providers::spec::ProviderSpec {
        id: PROVIDER_ID,
        name: PROVIDER_NAME,
        full_name: "cloudflare-ai",
        category: CATEGORY,
        base_url: BASE_URL,
        validate_url: "https://api.cloudflare.com/client/v4/user/tokens/verify",
        compatible_api: "openai-chat",
        supports_streaming: true,
        supports_tools: false,
        supports_vision: true,
        color: COLOR,
        icon_name: ICON_NAME,
        usage_url: None,
        quirks: Default::default(),
    }
}

#[derive(Debug, Clone)]
pub struct ModelDef { pub id: &'static str, pub name: &'static str, pub max_tokens: Option<u32>, pub context_length: u32, pub supports_vision: bool, pub supports_tools: bool }

pub const MODELS: &[ModelDef] = &[
    ModelDef { id: "@cf/zai-org/glm-5.2", name: "GLM 5.2", max_tokens: None, context_length: 128000, supports_vision: false, supports_tools: false },
];

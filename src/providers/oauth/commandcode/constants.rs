#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ModelDef {
    pub id: &'static str,
    pub backend_model: &'static str,
    pub max_tokens: Option<u32>,
    pub context_length: u32,
    pub vision: bool,
    pub reasoning: bool,
}

pub const PROVIDER_ID: &str = "cc";
pub const PROVIDER_NAME: &str = "Command Code";

#[allow(dead_code)]
pub const API_BASE_URL: &str = "https://api.commandcode.ai";
pub const CHAT_URL: &str = "https://api.commandcode.ai/alpha/generate";
pub const WHOAMI_URL: &str = "https://api.commandcode.ai/alpha/whoami";
pub const STUDIO_AUTH_URL: &str = "https://commandcode.ai/studio/auth/cli";

pub const CATEGORY: &str = "oauth";
pub const DEFAULT_TIMEOUT_SECS: u64 = 180;
pub const STREAM_FIRST_CHUNK_TIMEOUT_SECS: u64 = 200;
pub const STREAM_STALL_TIMEOUT_SECS: u64 = 360;

pub const COLOR: &str = "#0066FF";
pub const ICON_NAME: &str = "cc.png";

pub const USER_AGENT: &str = "cli";
pub const CLI_VERSION: &str = "1.87.1";
pub const CLI_ENVIRONMENT: &str = "production";

/// Seed model for Command Code.
pub const MODELS: &[ModelDef] = &[
    ModelDef { id: "deepseek/deepseek-v4.1-flash", backend_model: "deepseek/deepseek-v4.1-flash", max_tokens: Some(64000), context_length: 1000000, vision: true, reasoning: true },
];

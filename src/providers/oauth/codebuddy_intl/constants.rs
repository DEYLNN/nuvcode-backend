#[derive(Debug, Clone)]
pub struct ModelDef { pub id: &'static str, pub context_length: u32 }

pub const PROVIDER_ID: &str = "cbai";
pub const PROVIDER_NAME: &str = "CodeBuddy";
pub const API_BASE_URL: &str = "https://www.codebuddy.ai";
pub const CHAT_URL: &str = "https://www.codebuddy.ai/v2/chat/completions";
pub const STATE_URL: &str = "https://www.codebuddy.ai/v2/plugin/auth/state?platform=ide";
pub const TOKEN_URL: &str = "https://www.codebuddy.ai/v2/plugin/auth/token";
pub const REFRESH_URL: &str = "https://www.codebuddy.ai/v2/plugin/auth/token/refresh";
pub const CATEGORY: &str = "oauth";
pub const DEFAULT_TIMEOUT_SECS: u64 = 120;
pub const STREAM_FIRST_CHUNK_TIMEOUT_SECS: u64 = 200;
pub const STREAM_STALL_TIMEOUT_SECS: u64 = 360;
pub const COLOR: &str = "#006EFF";
pub const ICON_NAME: &str = "codebuddy.png";
pub const USER_AGENT: &str = "CLI/2.137.1 CodeBuddy/2.137.1";
pub const CLI_VERSION: &str = "2.137.1";

pub const MODELS: &[ModelDef] = &[
    ModelDef { id: "minimax-m3", context_length: 1000000 },
    ModelDef { id: "glm-5.2", context_length: 1000000 },
    ModelDef { id: "glm-5.3", context_length: 1000000 },
    ModelDef { id: "kimi-k3", context_length: 1000000 },
];
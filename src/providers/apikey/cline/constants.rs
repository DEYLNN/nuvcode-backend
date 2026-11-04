pub const PROVIDER_ID: &str = "cl";
pub const PROVIDER_NAME: &str = "Cline";
pub const CATEGORY: &str = "apikey";
pub const COLOR: &str = "#5B9BD5";
pub const ICON_NAME: &str = "cl.png";
pub const BASE_URL: &str = "https://api.cline.bot/api";
pub const DEFAULT_TIMEOUT_SECS: u64 = 64;
pub const STREAM_FIRST_CHUNK_TIMEOUT_SECS: u64 = 90;
pub const STREAM_STALL_TIMEOUT_SECS: u64 = 180;
pub const USER_AGENT: &str = "nuvcode/1.0";
/// Model which still thinks internally (Cline-side) but whose reasoning
/// content is suppressed downstream — only final answer reaches the agent.
pub const HIDE_REASONING_MODEL: &str = "cline-pass";

/// Model definition for Cline — `thinking_tags` lists tag names whose blocks
/// get stripped from response content (e.g. leaked Hermes internal tool calls,
/// or upstream-specific thinking markers). Opt-in per model; empty = no strip.
#[derive(Debug, Clone)]
pub struct ModelDef {
    pub id: &'static str,
    pub context_length: u32,
    pub thinking_tags: &'static [&'static str],
}

pub const MODELS: &[ModelDef] = &[
    ModelDef {
        id: "cline-pass/deepseek-v4-flash",
        context_length: 1000000,
        thinking_tags: &["ant_thinking", "thinking", "reasoning", "tool_calls", "tool", "DSML", "function_calls"],
    },
    ModelDef {
        id: "cline-pass/glm-5.3",
        context_length: 1000000,
        thinking_tags: &["ant_thinking", "thinking", "reasoning", "tool_calls", "tool", "DSML", "function_calls"],
    },
    ModelDef {
        id: "cline-pass/minimax-m3",
        context_length: 1000000,
        thinking_tags: &["ant_thinking", "thinking", "reasoning", "tool_calls", "tool", "DSML", "function_calls"],
    },
];

/// Look up thinking_tags for a model id. Empty slice if model unknown / no tags.
pub fn thinking_tags_for(model_id: &str) -> &'static [&'static str] {
    for m in MODELS {
        if m.id == model_id {
            return m.thinking_tags;
        }
    }
    &[]
}

pub fn provider_spec() -> crate::providers::spec::ProviderSpec {
    crate::providers::spec::ProviderSpec {
        id: PROVIDER_ID,
        name: PROVIDER_NAME,
        full_name: "cline",
        category: CATEGORY,
        base_url: BASE_URL,
        validate_url: "https://api.cline.bot/api/v1/models",
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

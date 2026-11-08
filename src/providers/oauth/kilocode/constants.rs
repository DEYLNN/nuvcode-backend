#[derive(Debug, Clone)]
pub struct ModelDef {
    pub id: &'static str,
    pub backend_model: &'static str,
    pub max_tokens: Option<u32>,
    pub context_length: u32,
}

pub const PROVIDER_ID: &str = "kc";
pub const PROVIDER_NAME: &str = "Kilo Code";

pub const API_BASE_URL: &str = "https://api.kilo.ai";
pub const CHAT_URL: &str = "https://api.kilo.ai/api/openrouter/chat/completions";

pub const CATEGORY: &str = "oauth";
pub const DEFAULT_TIMEOUT_SECS: u64 = 120;
pub const STREAM_FIRST_CHUNK_TIMEOUT_SECS: u64 = 120;
pub const STREAM_STALL_TIMEOUT_SECS: u64 = 120;

pub const COLOR: &str = "#FF6B35";
pub const ICON_NAME: &str = "kc.png";

/// Seed models for Kilo Code.
///
/// Deliberately one entry, not a catalogue. `list_models()` already merges whatever the
/// operator adds in `custom_models` (admin UI → DB), so anything beyond this seed is
/// data, not code — adding a model should not need a rebuild.
///
/// This one is the seed because it is the model this gateway was built against: the
/// default free tier with the largest context, and the one that is always worth having
/// even on a fresh install with an empty database.
pub const MODELS: &[ModelDef] = &[
    ModelDef { id: "nemotron-3.5-super-vl:free", backend_model: "nvidia/nemotron-3.5-super-vl:free", max_tokens: None, context_length: 1000000 },
];

// Provider-agnostic usage tracking.
//
// Two layers:
// 1. `canonicalize_usage(raw)` extracts token counts from any upstream
//    usage shape (OpenAI, Anthropic, Gemini). Future engines pass raw
//    JSON here and get back `{prompt, completion}` ints.
// 2. `UsageTracker::save(entry)` writes one row to the `usage` table.
//    Wrap with `record_request(...)` for the common case: build the
//    entry from a `provider_id`, `model_id`, etc + raw usage value.
//
// Adds 2026-07-28: `ttft_ms` (streaming-only) and `provider_api_key_id`
// (which underlying provider key handled the request).

use std::time::Instant;
use serde_json::Value;
use sqlx::PgPool;
use tokio::sync::broadcast;

use crate::db::{self, UsageEntry};
use crate::state::UsageBroadcast;

#[derive(Clone)]
pub struct UsageTracker {
    pool: PgPool,
    /// Optional broadcast channel — when set, every successful save also
    /// publishes to SSE subscribers. `None` is fine for tests / minimal
    /// setups; admin SSE just won't receive push updates.
    broadcast: Option<UsageBroadcast>,
}

impl UsageTracker {
    pub fn new(pool: PgPool) -> Self {
        Self { pool, broadcast: None }
    }

    /// Wire the broadcast sender (called by `AppState::new`).
    pub fn with_broadcast(mut self, tx: UsageBroadcast) -> Self {
        self.broadcast = Some(tx);
        self
    }

    /// Save a usage entry. Tracking must never break the upstream API call,
    /// so any DB error is logged & swallowed here rather than propagated.
    /// After successful insert, publishes to broadcast channel (best-effort).
    pub async fn save(&self, entry: UsageEntry) {
        let pool = self.pool.clone();
        db::save_request_usage(&pool, &entry).await;
        tracing::debug!(
            "usage saved: {}/{} prompt={} completion={} status={}",
            entry.provider_id, entry.model_id,
            entry.prompt_tokens, entry.completion_tokens,
            entry.status
        );

        // Publish the most-recent row to subscribers (poll-free path).
        if let Some(tx) = &self.broadcast {
            if let Some(row) = db::fetch_latest_usage(&pool).await {
                let _ = tx.send(row);
            }
        }
    }
}

/// Normalized shape returned by [`canonicalize_usage`].
#[derive(Debug, Default, Clone, Copy)]
pub struct CanonicalUsage {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
}

/// Extract token counts from any upstream usage block.
///
/// Recognizes:
/// - OpenAI / OpenAI-compatible (incl. o-series): `usage.prompt_tokens`,
///   `usage.completion_tokens`. Cached/reasoning variants ignored.
pub fn canonicalize_usage(raw: Option<&Value>) -> CanonicalUsage {
    let Some(usage) = raw else {
        return CanonicalUsage::default();
    };
    let prompt = usage
        .get("prompt_tokens")
        .or_else(|| usage.get("input_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let completion = usage
        .get("completion_tokens")
        .or_else(|| usage.get("output_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    CanonicalUsage {
        prompt_tokens: prompt.max(0),
        completion_tokens: completion.max(0),
    }
}

/// Helper for the success path of a streaming or non-streaming request.
///
/// `started.elapsed()` fills `latency_ms`; `ttft_ms` should be `Some` only
/// when the caller knows the first chunk's arrival time (i.e. streaming).
///
/// `provider_api_key_id` is the underlying provider key that actually
/// handled the request — empty string if the engine doesn't track it.
pub async fn record_success(
    tracker: &UsageTracker,
    started: Instant,
    provider_id: &str,
    model_id: &str,
    gateway_key_id: &str,
    provider_api_key_id: Option<&str>,
    endpoint: &str,
    usage: CanonicalUsage,
    ttft_ms: Option<i64>,
) {
    tracker
        .save(UsageEntry {
            provider_id: provider_id.into(),
            model_id: model_id.into(),
            gateway_key_id: gateway_key_id.into(),
            provider_api_key_id: provider_api_key_id.map(str::to_string),
            endpoint: endpoint.into(),
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            latency_ms: started.elapsed().as_millis() as i64,
            ttft_ms,
            status: "success".into(),
            status_code: 200,
            error_message: None,
            request_body: None,
            response_body: None,
        })
        .await;
}

/// Helper for the failure path of any chat request. Token counts default
/// to 0 (we don't know what came back) and `ttft_ms` is `None`.
pub async fn record_error(
    tracker: &UsageTracker,
    started: Instant,
    provider_id: &str,
    model_id: &str,
    gateway_key_id: &str,
    provider_api_key_id: Option<&str>,
    endpoint: &str,
    status_code: i64,
    error_message: &str,
) {
    tracker
        .save(UsageEntry {
            provider_id: provider_id.into(),
            model_id: model_id.into(),
            gateway_key_id: gateway_key_id.into(),
            provider_api_key_id: provider_api_key_id.map(str::to_string),
            endpoint: endpoint.into(),
            prompt_tokens: 0,
            completion_tokens: 0,
            latency_ms: started.elapsed().as_millis() as i64,
            ttft_ms: None,
            status: "error".into(),
            status_code,
            error_message: Some(error_message.to_string()),
            request_body: None,
            response_body: None,
        })
        .await;
}

/// Streams use this to accumulate tokens across SSE chunks and write one
/// row at stream finalize time. The `ttft_ms` field is set on the first
/// chunk that carries a usage payload (or by the caller if it tracks
/// first-chunk arrival).
#[derive(Default)]
pub struct StreamRecorder {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub ttft_ms: Option<i64>,
}

impl StreamRecorder {
    pub fn record_chunk(&mut self, started_at: Instant, prompt: Option<i64>, completion: Option<i64>) {
        if self.ttft_ms.is_none() {
            // First chunk we record tokens for = first-chunk arrival.
            self.ttft_ms = Some(started_at.elapsed().as_millis() as i64);
        }
        if let Some(p) = prompt {
            self.prompt_tokens += p;
        }
        if let Some(c) = completion {
            self.completion_tokens += c;
        }
    }
}

use std::sync::Arc;
use std::time::Instant;

use axum::Json;
use axum::response::IntoResponse;

use crate::error::GatewayError;
use crate::middleware::auth::GatewayKeyInfo;
use crate::providers::manager::ProviderManager;
use crate::services::usage_tracking::{CanonicalUsage, record_error, record_success};
use crate::state::AppState;
use crate::types::chat::ChatCompletionRequest;

struct ComboConfig {
    tiers: Vec<String>,
    strategy: String,
    #[allow(dead_code)]
    min_context: i64,
}

async fn fetch_combo(db: &sqlx::PgPool, combo_name: &str) -> Result<ComboConfig, GatewayError> {
    let row = sqlx::query_as::<_, (String, String, bool, i64)>(
        "SELECT tiers, strategy, is_active, min_context FROM combos WHERE name = $1 OR id = $2"
    )
    .bind(combo_name)
    .bind(combo_name)
    .fetch_optional(db)
    .await
    .map_err(|_| GatewayError::Internal("DB error".into()))?
    .ok_or_else(|| GatewayError::ModelNotFound {
        provider: "combo".to_string(),
        model: combo_name.to_string(),
    })?;

    let (tiers_str, strategy, is_active, min_context) = row;
    tracing::info!(
        "COMBO FETCH: name={} strategy={} is_active={} min_context={}",
        combo_name, strategy, is_active, min_context
    );
    if !is_active {
        tracing::warn!("COMBO FETCH: combo is_active=false, rejecting: {}", combo_name);
        return Err(GatewayError::ModelNotFound {
            provider: "combo".to_string(),
            model: combo_name.to_string(),
        });
    }

    let tiers: Vec<String> = serde_json::from_str(&tiers_str)
        .map_err(|e| {
            tracing::error!("COMBO FETCH: invalid tiers JSON: {} err={}", tiers_str, e);
            GatewayError::Internal("Invalid combo tiers".into())
        })?;

    tracing::info!("COMBO FETCH: {} tiers resolved: {:?}", combo_name, tiers);

    if tiers.is_empty() {
        return Err(GatewayError::Internal("Combo has no tiers".into()));
    }

    Ok(ComboConfig { tiers, strategy, min_context })
}

// ── Token estimation ──

/// Estimate input tokens using cl100k_base BPE.
/// Sums role + content tokens, adds ~4 tokens overhead per message.
pub(crate) fn estimate_input_tokens(request: &ChatCompletionRequest) -> usize {
    let bpe = match tiktoken_rs::cl100k_base() {
        Ok(b) => b,
        Err(_) => return 0,
    };
    let mut total = 0usize;
    for msg in &request.messages {
        total += bpe.encode_with_special_tokens(&msg.role).len();
        if let Some(ref content) = msg.content {
            total += bpe.encode_with_special_tokens(content).len();
        }
        total += 4; // overhead: role tags, separators
    }
    total
}

// ── Tier context lookup ──

/// Look up context_length for a tier model from the provider model list.
pub(crate) async fn get_tier_ctx(pm: &ProviderManager, tier_model: &str) -> Option<u32> {
    let models = pm.list_all_models().await;
    models
        .iter()
        .find(|m| m.id == tier_model)
        .and_then(|m| m.context_length)
        // If model not found in registry, return None — caller treats as "unknown ctx, don't skip"
}

/// Get the combo's virtual context_length based on strategy.
/// fallback & round_robin → min(ctx tiers) — conservative, avoid overflow.
/// balanced → max(ctx tiers) — will pick the tier that fits.
pub(crate) async fn combo_ctx(pm: &ProviderManager, tiers: &[String], strategy: &str) -> u32 {
    let mut ctxs = Vec::with_capacity(tiers.len());
    for tier in tiers {
        ctxs.push(get_tier_ctx(pm, tier).await.unwrap_or(0));
    }
    let known: Vec<u32> = ctxs.iter().copied().filter(|&c| c > 0).collect();
    if known.is_empty() {
        return 0;
    }
    if strategy == "balanced" {
        *known.iter().max().unwrap_or(&0)
    } else {
        // fallback & round_robin → min ctx
        *known.iter().min().unwrap_or(&0)
    }
}

// ── Strategy: compute tier order ──

/// Compute the tier index order based on strategy.
/// - fallback: DB order, no skip. combo ctx = min(ctx tiers).
/// - round_robin: shuffled, skip ctx < estimate. combo ctx = min(ctx tiers).
/// - balanced: two-step — (1) keep only tiers whose ctx fits the estimate,
///   (2) among those, the tightest fitting ctx goes first (its peers next in DB
///   order), then the rest by ctx ascending as overflow fallbacks.
///   combo ctx = max(ctx tiers).
async fn compute_tier_order(
    pm: &ProviderManager,
    cfg: &ComboConfig,
    estimated_tokens: usize,
) -> Vec<usize> {
    let ctxs: Vec<u32> = {
        let mut v = Vec::with_capacity(cfg.tiers.len());
        for tier in &cfg.tiers {
            v.push(get_tier_ctx(pm, tier).await.unwrap_or(0));
        }
        v
    };
    order_tiers(&ctxs, &cfg.strategy, estimated_tokens)
}

/// Pure tier-ordering logic (no DB, no provider manager) so it can be unit
/// tested. `ctxs[i]` is the context window of `tiers[i]`; 0 means unknown.
fn order_tiers(ctxs: &[u32], strategy: &str, estimated_tokens: usize) -> Vec<usize> {
    let mut order: Vec<usize> = (0..ctxs.len()).collect();

    match strategy {
        "round_robin" => {
            use rand::seq::SliceRandom;
            order.shuffle(&mut rand::thread_rng());
            // Skip tiers where ctx is known (>0) and smaller than estimated_tokens.
            if estimated_tokens > 0 {
                order.retain(|&i| ctxs[i] == 0 || ctxs[i] as usize >= estimated_tokens);
            }
        }
        "balanced" => {
            // Step 1 — eligibility: a tier must fit the prompt. Unknown ctx (0)
            // stays eligible: we cannot prove it does not fit.
            if estimated_tokens > 0 {
                order.retain(|&i| ctxs[i] == 0 || ctxs[i] as usize >= estimated_tokens);
            }

            if order.is_empty() {
                // Nothing fits. Fall back to the whole list so the request can
                // still fail upstream with a real error instead of a bare 400.
                order = (0..ctxs.len()).collect();
            } else {
                // Step 2 — pick the tightest fit that still holds the prompt:
                // the smallest ctx >= estimate. Tiers sharing that ctx are
                // preferred peers and keep DB order among themselves. Everything
                // else follows by ctx ascending (small overflow risk first),
                // with unknown-ctx tiers last.
                let tight = order
                    .iter()
                    .map(|&i| ctxs[i])
                    .filter(|&c| c >= estimated_tokens as u32)
                    .min();
                let tight = tight.unwrap_or(0);

                let rank = |i: usize| -> (u8, u32, usize) {
                    let c = ctxs[i];
                    if tight > 0 && c == tight {
                        (0, c, i) // preferred: exact ctx match, DB order
                    } else if c == 0 {
                        (2, u32::MAX, i) // unknown ctx last
                    } else {
                        (1, c, i) // other fitting ctx, smallest first
                    }
                };
                order.sort_by_key(|&i| rank(i));
            }
        }
        _ => {
            // "fallback" — keep DB order, no skip
        }
    }

    order
}

/// Handle combo/xxx requests — non-streaming.
/// Resolves combo to real provider+model, dispatches. Usage tracked under real provider.
pub(crate) async fn handle_combo_request(
    state: Arc<AppState>,
    gw_key: GatewayKeyInfo,
    request: ChatCompletionRequest,
    combo_name: String,
    start: Instant,
) -> Result<axum::response::Response, GatewayError> {
    let cfg = fetch_combo(&state.db, &combo_name).await?;

    // Resolve all tiers under read lock, clone Arc, release lock before any HTTP call
    let tier_providers: Vec<(usize, String, String, String, Arc<dyn crate::providers::traits::Provider>)> = {
        let pm = state.provider_manager.read().await;
        let estimated = if cfg.strategy == "balanced" {
            estimate_input_tokens(&request)
        } else {
            0
        };
        let tier_order = compute_tier_order(&pm, &cfg, estimated).await;

        tracing::info!(
            "COMBO: name={} strategy={} tiers={:?} estimated_tokens={} order={:?}",
            combo_name, cfg.strategy, cfg.tiers, estimated, tier_order
        );

        let mut resolved = Vec::new();
        for idx in tier_order {
            let tier_model = &cfg.tiers[idx];
            let (raw_prefix, model_name) = match tier_model.split_once('/') {
                Some((p, m)) => (p, m),
                None => continue,
            };
            let provider_id = match pm.resolve_provider_id(raw_prefix) {
                Some(id) => id,
                None => {
                    tracing::warn!("COMBO: resolve_provider_id failed for raw_prefix={:?}", raw_prefix);
                    continue;
                }
            };
            let provider = match pm.get(&provider_id) {
                Some(p) => p,
                None => {
                    tracing::warn!("COMBO: provider not found in active map: {:?}", provider_id);
                    continue;
                }
            };
            resolved.push((idx, tier_model.clone(), model_name.to_string(), provider_id.clone(), provider));
        }
        resolved
    }; // read lock released here

    let mut last_error = None;
    for (idx, tier_model, model_name, provider_id, provider) in tier_providers {
        let mut tier_req = request.clone();
        tier_req.model = model_name.clone();

        tracing::info!("COMBO: sending to provider model={}", tier_req.model);

        match provider.chat_completion(tier_req).await {
            Ok(result) => {
                tracing::info!("COMBO: tier[{}] OK from provider={}", idx, provider_id);
                let usage = match &result.response.usage {
                    Some(u) => CanonicalUsage {
                        prompt_tokens: u.prompt_tokens as i64,
                        completion_tokens: u.completion_tokens as i64,
                    },
                    None => CanonicalUsage::default(),
                };
                record_success(
                    &state.usage_tracker,
                    start,
                    &provider_id,
                    &tier_model,
                    &gw_key.key_id,
                    result.used_key_id.as_deref(),
                    "/v1/chat/completions",
                    usage,
                    None,
                ).await;
                return Ok(Json(result.response).into_response());
            }
            Err(e) => {
                tracing::warn!("COMBO: tier[{}] FAILED provider={} model={} error={}", idx, provider_id, model_name, e);
                let status = e.http_status().unwrap_or(502);
                record_error(
                    &state.usage_tracker,
                    start,
                    &provider_id,
                    &tier_model,
                    &gw_key.key_id,
                    None,
                    "/v1/chat/completions",
                    status as i64,
                    &e.to_string(),
                ).await;
                last_error = Some(e);
            }
        }
    }

    let err_msg = match last_error {
        Some(e) => format!("All combo tiers failed. Last error: {}", e),
        None => "All combo tiers failed: no tiers defined or all skipped".into(),
    };
    Err(GatewayError::ProviderError(err_msg))
}

/// Handle combo/xxx streaming requests.
/// Resolves to a real provider and delegates to the standard streaming handler.
pub(crate) async fn handle_combo_request_stream(
    state: Arc<AppState>,
    gw_key: GatewayKeyInfo,
    request: ChatCompletionRequest,
    combo_name: String,
    start: Instant,
) -> Result<axum::response::Response, GatewayError> {
    let cfg = fetch_combo(&state.db, &combo_name).await?;

    // Resolve all tiers under read lock, clone Arc, release lock before any HTTP call
    let tier_providers: Vec<(usize, String, String, String, Arc<dyn crate::providers::traits::Provider>)> = {
        let pm = state.provider_manager.read().await;
        let estimated = if cfg.strategy == "balanced" {
            estimate_input_tokens(&request)
        } else {
            0
        };
        let tier_order = compute_tier_order(&pm, &cfg, estimated).await;

        tracing::info!(
            "COMBO STREAM: name={} strategy={} tiers={:?} estimated_tokens={} order={:?}",
            combo_name, cfg.strategy, cfg.tiers, estimated, tier_order
        );

        let mut resolved = Vec::new();
        for idx in tier_order {
            let tier_model = &cfg.tiers[idx];
            let (raw_prefix, model_name) = match tier_model.split_once('/') {
                Some((p, m)) => (p, m),
                None => continue,
            };
            let provider_id = match pm.resolve_provider_id(raw_prefix) {
                Some(id) => id,
                None => continue,
            };
            let provider = match pm.get(&provider_id) {
                Some(p) => p,
                None => continue,
            };
            resolved.push((idx, tier_model.clone(), model_name.to_string(), provider_id.clone(), provider));
        }
        resolved
    }; // read lock released here

    let mut last_error = None;
    for (idx, tier_model, model_name, provider_id, provider) in tier_providers {
        let mut tier_req = request.clone();
        tier_req.model = model_name.clone();
        tier_req.stream = Some(true);
        if tier_req.stream_options.is_none() {
            tier_req.stream_options = Some(serde_json::json!({"include_usage": true}));
        }

        // provider_id goes in the provider_id slot (usage attribution); the full
        // tier id ("bjt/model") is the model column.
        match crate::api::chat::streaming::handle_streaming(
            &state, &gw_key, provider.as_ref(), provider_id.as_str(), tier_model.as_str(), &tier_req, start,
        ).await {
            Ok(resp) => {
                return Ok(resp);
            }
            Err(e) => {
                tracing::warn!("COMBO STREAM: tier[{}] FAILED model={} error={}", idx, model_name, e);
                last_error = Some(e);
            }
        }
    }

    let err_msg = match last_error {
        Some(e) => format!("All combo tiers failed. Last error: {}", e),
        None => "All combo tiers failed: no tiers defined or all skipped".into(),
    };
    Err(GatewayError::ProviderError(err_msg))
}

#[cfg(test)]
mod tests {
    use super::order_tiers;

    // bjt-test shape: four 1M tiers + one 198K tier.
    const CTXS: &[u32] = &[1048576, 1048576, 1048576, 1048576, 198000];

    #[test]
    fn balanced_prefers_smallest_ctx_that_fits() {
        // A 100K prompt fits everywhere. The tightest fit is the 198K tier,
        // so it must be picked first instead of a random 1M tier.
        let order = order_tiers(CTXS, "balanced", 100_000);
        let first = order[0];
        assert_eq!(
            CTXS[first], 198000,
            "expected tightest fitting ctx (198000) first, got index {first} ctx {}",
            CTXS[first]
        );
    }

    #[test]
    fn balanced_drops_tiers_too_small_for_prompt() {
        // 500K prompt cannot fit the 198K tier at all.
        let order = order_tiers(CTXS, "balanced", 500_000);
        for &i in &order {
            assert!(
                CTXS[i] == 0 || CTXS[i] as usize >= 500_000,
                "ineligible tier kept: index {i} ctx {}",
                CTXS[i]
            );
        }
        assert_eq!(order.len(), 4, "expected the four 1M tiers only");
    }

    #[test]
    fn balanced_keeps_all_tiers_when_nothing_fits() {
        // Prompt larger than every ctx: keep everything so upstream returns a
        // real context-overflow error instead of us inventing a 400.
        let order = order_tiers(CTXS, "balanced", 5_000_000);
        assert_eq!(order.len(), CTXS.len());
    }

    #[test]
    fn balanced_unknown_ctx_goes_last() {
        let ctxs = [0u32, 198_000, 1_048_576];
        let order = order_tiers(&ctxs, "balanced", 1000);
        assert_eq!(ctxs[order[0]], 198_000, "tightest fit must lead");
        assert_eq!(*order.last().unwrap(), 0, "unknown ctx must be last");
    }

    #[test]
    fn fallback_keeps_db_order() {
        let order = order_tiers(CTXS, "fallback", 100_000);
        assert_eq!(order, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn round_robin_drops_ineligible_but_keeps_count() {
        let order = order_tiers(CTXS, "round_robin", 500_000);
        assert_eq!(order.len(), 4, "198K tier must be dropped");
        assert!(!order.contains(&4), "index 4 (198K) not eligible");
    }

    #[test]
    fn balanced_three_tiers_1m_1m_256k_with_300k_prompt() {
        // The scenario asked about: ctx = [1M, 1M, 256K], estimate 300K.
        let ctxs = [1_048_576u32, 1_048_576, 262_144];
        let order = order_tiers(&ctxs, "balanced", 300_000);

        // 256K cannot hold a 300K prompt: it must be dropped entirely, so an
        // error on T1 falls through to T2 — never to the 256K tier.
        assert_eq!(order, vec![0, 1], "expected only the two 1M tiers, DB order");
        assert!(!order.contains(&2), "256K tier must be ineligible at 300K");

        // Both 1M tiers are the tightest fit, so they keep DB order.
        assert_eq!(ctxs[order[0]], 1_048_576);
        assert_eq!(ctxs[order[1]], 1_048_576);
    }

    #[test]
    fn balanced_three_tiers_1m_1m_256k_with_small_prompt() {
        // Same tiers, small prompt: 256K is now the tightest fit and leads.
        let ctxs = [1_048_576u32, 1_048_576, 262_144];
        let order = order_tiers(&ctxs, "balanced", 1_000);
        assert_eq!(order[0], 2, "256K is the tightest fit and must lead");
        assert_eq!(order.len(), 3, "all tiers eligible for a small prompt");
    }
}

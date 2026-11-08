use sqlx::PgPool;

use crate::db;
use crate::error::GatewayError;
use crate::middleware::auth::GatewayKeyInfo;

/// Layer 1 + Layer 2: Check if model is allowed for this gateway key.
/// - Layer 1 (global): already done before calling this (disabled_models)
/// - Layer 2 (per-key):
pub async fn check_model_access(
    gw_key: &GatewayKeyInfo,
    model: &str,
) -> Result<(), GatewayError> {
    match gw_key.access_type.as_str() {
        "allow" => {
            if !gw_key.allowed_models.contains(&model.to_string()) {
                return Err(GatewayError::ModelNotFound {
                    provider: "gateway".to_string(),
                    model: model.to_string(),
                });
            }
        }
        "deny" => {
            if gw_key.allowed_models.contains(&model.to_string()) {
                return Err(GatewayError::ModelNotFound {
                    provider: "gateway".to_string(),
                    model: model.to_string(),
                });
            }
        }
        _ => {} // "full" = all models
    }
    Ok(())
}

/// Backward-compat thin wrapper. New code should call
/// `services::usage_tracking::UsageTracker::save` directly.
#[allow(dead_code)]
pub async fn track_gateway_usage(
    db: &PgPool,
    gateway_key_id: &str,
    provider_id: &str,
    model_id: &str,
    prompt_tokens: i64,
    completion_tokens: i64,
    latency_ms: i64,
) {
    db::save_request_usage(
        db,
        &db::UsageEntry {
            provider_id: provider_id.into(),
            model_id: model_id.into(),
            gateway_key_id: gateway_key_id.into(),
            endpoint: "/v1/chat/completions".into(),
            prompt_tokens,
            completion_tokens,
            latency_ms,
            status: "success".into(),
            status_code: 200,
            error_message: None,
            provider_api_key_id: None,
            ttft_ms: None,
            request_body: None,
            response_body: None,
        },
    )
    .await;
}

/// Layer 2: Check if gateway key has exceeded its max_tokens limit.
/// max_tokens=0 means unlimited. Single JOIN query.
pub async fn check_token_limit(
    db: &PgPool,
    gateway_key_id: &str,
) -> Result<(), GatewayError> {
    let row: Option<(i64, i64)> = sqlx::query_as(
        "SELECT gk.max_tokens, COALESCE(SUM(u.total_tokens), 0) FROM gateway_keys gk LEFT JOIN usage u ON u.gateway_key_id = gk.id WHERE gk.id = $1 GROUP BY gk.id"
    )
    .bind(gateway_key_id)
    .fetch_optional(db)
    .await
    .unwrap_or(None);

    let (max_tokens, used) = match row {
        Some((max, used)) => (max, used),
        None => return Ok(()), // key not found, shouldn't happen in practice
    };

    if max_tokens <= 0 { return Ok(()); } // 0 = unlimited

    if used >= max_tokens {
        return Err(GatewayError::TokenLimitExceeded { used, max: max_tokens });
    }

    Ok(())
}

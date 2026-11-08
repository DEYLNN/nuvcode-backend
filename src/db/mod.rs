pub mod migrations;
pub mod models;

use sqlx::postgres::{PgPool, PgPoolOptions};

/// Pool sizing. `PgPool::connect` defaults to max 10 connections, which is far
/// below PostgreSQL's own default `max_connections = 100` — under real gateway
/// traffic (many concurrent chat requests each needing a pool slot) requests
/// would queue waiting for a connection instead of being served.
///
/// Sized against the DB's actual budget, not a guessed number:
///   - max 100  = one connection per DB slot, so the pool is never the bottleneck.
///   - min 10   = a warm pool at boot, so a traffic spike doesn't pay the
///                connection handshake on the first N requests.
///   - acquire_timeout 5s = a clear error instead of hanging forever when the
///     pool is genuinely exhausted (better to fail one request than to let
///     every request pile up behind it).
const DB_MAX_CONNECTIONS: u32 = 100;
const DB_MIN_CONNECTIONS: u32 = 10;
const DB_ACQUIRE_TIMEOUT_SECS: u64 = 5;

/// Initialize database — run migrations, return pool.
pub async fn init(database_url: &str) -> anyhow::Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(DB_MAX_CONNECTIONS)
        .min_connections(DB_MIN_CONNECTIONS)
        .acquire_timeout(std::time::Duration::from_secs(DB_ACQUIRE_TIMEOUT_SECS))
        .connect(database_url)
        .await?;
    migrations::run(&pool).await?;
    Ok(pool)
}

/// Load active API keys for a provider
pub async fn load_provider_keys(pool: &PgPool, provider_id: &str) -> anyhow::Result<Vec<models::ApiKey>> {
    let keys = sqlx::query_as::<_, models::ApiKey>(
        "SELECT id, provider_id, key_value, label, is_active, last_used_at, consecutive_use_count, consecutive_error_count, created_at, updated_at
         FROM api_keys WHERE provider_id = $1 AND is_active = true ORDER BY created_at"
    )
    .bind(provider_id)
    .fetch_all(pool)
    .await?;

    Ok(keys)
}

/// Count keys for a provider. `active_only = true` ignores inactive (locked) rows.
pub async fn count_provider_keys(pool: &PgPool, provider_id: &str, active_only: bool) -> i64 {
    let sql = if active_only {
        "SELECT COUNT(*) FROM api_keys WHERE provider_id = $1 AND is_active = true"
    } else {
        "SELECT COUNT(*) FROM api_keys WHERE provider_id = $1"
    };
    sqlx::query_scalar(sql)
        .bind(provider_id)
        .fetch_one(pool)
        .await
        .unwrap_or(0)
}

/// Count active keys across all providers.
#[allow(dead_code)]
pub async fn count_total_active_keys(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM api_keys WHERE is_active = true")
        .fetch_one(pool)
        .await
        .unwrap_or(0)
}

/// Per-provider key counts — used by /admin/api/keys/stats for the FE provider
/// dropdown. Single GROUP BY scan, no pagination dependency. Returns
/// (provider_id, total_count, active_count) sorted alphabetically.
pub async fn count_keys_per_provider(pool: &PgPool) -> Vec<(String, i64, i64)> {
    sqlx::query_as::<_, (String, i64, i64)>(
        "SELECT provider_id, COUNT(*), SUM(CASE WHEN is_active = true THEN 1 ELSE 0 END) \
         FROM api_keys GROUP BY provider_id ORDER BY provider_id"
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default()
}

/// Count distinct (provider_id, key_value) groups that have >1 non-OAuth
/// duplicate row — used by FE to display "X duplicates" badge next to Run.
/// Same scope as api_dedupe_keys (non-OAuth only).
pub async fn count_duplicate_groups(pool: &PgPool, provider_id: Option<&str>) -> i64 {
    let q = if provider_id.is_some() {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM ( \
               SELECT provider_id, key_value \
               FROM api_keys \
               WHERE COALESCE(key_type, 'apikey') != 'oauth' AND provider_id = $1 \
               GROUP BY provider_id, key_value \
               HAVING COUNT(*) > 1\
             )"
        )
        .bind(provider_id.unwrap_or(""))
    } else {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM ( \
               SELECT provider_id, key_value \
               FROM api_keys \
               WHERE COALESCE(key_type, 'apikey') != 'oauth' \
               GROUP BY provider_id, key_value \
               HAVING COUNT(*) > 1\
             )"
        )
    };
    q.fetch_one(pool).await.unwrap_or(0)
}

// ── Usage tracking ──
//
// One row per upstream API call. Populated by services::usage_tracking
// via UsageTracker. Engines (openai_compat, anthropic_compat, custom
// providers) call UsageTracker::save() after their response/stream
// finalizes. Aggregations power both the admin Usage page and the
// gateway key `max_tokens` quota check (see services/gateway.rs).

#[derive(Debug, Clone)]
pub struct UsageEntry {
    pub provider_id: String,
    pub model_id: String,
    pub gateway_key_id: String,
    pub provider_api_key_id: Option<String>,
    pub endpoint: String,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub latency_ms: i64,
    /// Time-to-first-token in milliseconds. Only populated for streaming.
    pub ttft_ms: Option<i64>,
    pub status: String,
    pub status_code: i64,
    pub error_message: Option<String>,
    pub request_body: Option<String>,
    pub response_body: Option<String>,
}

pub async fn save_request_usage(pool: &PgPool, e: &UsageEntry) {
    let total = e.prompt_tokens + e.completion_tokens;
    let _ = sqlx::query(
        "INSERT INTO usage (id, provider_id, model_id, gateway_key_id, status, status_code,
                            prompt_tokens, completion_tokens, total_tokens, latency_ms,
                            endpoint, error_message, request_body, response_body,
                            provider_api_key_id, ttft_ms)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)",
    )
    .bind(format!("usage_{}", uuid::Uuid::new_v4().simple()))
    .bind(&e.provider_id)
    .bind(&e.model_id)
    .bind(&e.gateway_key_id)
    .bind(&e.status)
    .bind(e.status_code)
    .bind(e.prompt_tokens)
    .bind(e.completion_tokens)
    .bind(total)
    .bind(e.latency_ms)
    .bind(&e.endpoint)
    .bind(&e.error_message)
    .bind(&e.request_body)
    .bind(&e.response_body)
    .bind(&e.provider_api_key_id)
    .bind(e.ttft_ms)
    .execute(pool)
    .await;
}

#[derive(Debug, serde::Serialize, sqlx::FromRow)]
pub struct UsageStatsRow {
    pub total_requests: i64,
    pub total_prompt_tokens: i64,
    pub total_completion_tokens: i64,
    pub total_tokens: i64,
}

pub async fn usage_stats(pool: &PgPool) -> UsageStatsRow {
    sqlx::query_as::<_, UsageStatsRow>(
        "SELECT
            COUNT(*)::BIGINT AS total_requests,
            COALESCE(SUM(prompt_tokens), 0)::BIGINT AS total_prompt_tokens,
            COALESCE(SUM(completion_tokens), 0)::BIGINT AS total_completion_tokens,
            COALESCE(SUM(total_tokens), 0)::BIGINT AS total_tokens
         FROM usage",
    )
    .fetch_one(pool)
    .await
    .unwrap_or(UsageStatsRow {
        total_requests: 0,
        total_prompt_tokens: 0,
        total_completion_tokens: 0,
        total_tokens: 0,
    })
}

#[derive(Debug, serde::Serialize, sqlx::FromRow)]
pub struct UsagePerKeyRow {
    pub gateway_key_id: String,
    /// Label of the gateway key (admin-issued) — `None` when the row has no
    /// `gateway_key_id` or the key was deleted. Rendered in the Per-Key card.
    pub label: Option<String>,
    /// Truncated/masked key value for at-a-glance identification in admin UI.
    pub key_value: Option<String>,
    pub requests: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
}

pub async fn usage_per_key(pool: &PgPool) -> Vec<UsagePerKeyRow> {
    sqlx::query_as::<_, UsagePerKeyRow>(
        "SELECT
            u.gateway_key_id,
            gk.label AS label,
            gk.key_value AS key_value,
            COUNT(*)::BIGINT AS requests,
            COALESCE(SUM(u.prompt_tokens), 0)::BIGINT AS prompt_tokens,
            COALESCE(SUM(u.completion_tokens), 0)::BIGINT AS completion_tokens,
            COALESCE(SUM(u.total_tokens), 0)::BIGINT AS total_tokens
         FROM usage u
         LEFT JOIN gateway_keys gk ON gk.id = u.gateway_key_id
         GROUP BY u.gateway_key_id, gk.label, gk.key_value
         ORDER BY requests DESC",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default()
}

/// All-time rollup per provider — powers the Dashboard "Top Providers" card.
#[derive(Debug, serde::Serialize, sqlx::FromRow)]
pub struct UsagePerProviderRow {
    pub provider_id: String,
    pub requests: i64,
    pub success: i64,
    pub errors: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    /// Mean latency in ms across rows that reported one; 0 when none did.
    pub avg_latency_ms: i64,
}

pub async fn usage_per_provider(pool: &PgPool) -> Vec<UsagePerProviderRow> {
    sqlx::query_as::<_, UsagePerProviderRow>(
        "SELECT
            COALESCE(u.provider_id, 'unknown') AS provider_id,
            COUNT(*)::BIGINT AS requests,
            COALESCE(SUM(CASE WHEN u.status = 'error' THEN 0 ELSE 1 END), 0)::BIGINT AS success,
            COALESCE(SUM(CASE WHEN u.status = 'error' THEN 1 ELSE 0 END), 0)::BIGINT AS errors,
            COALESCE(SUM(u.prompt_tokens), 0)::BIGINT AS prompt_tokens,
            COALESCE(SUM(u.completion_tokens), 0)::BIGINT AS completion_tokens,
            COALESCE(SUM(u.total_tokens), 0)::BIGINT AS total_tokens,
            COALESCE(CAST(AVG(NULLIF(u.latency_ms, 0)) AS BIGINT), 0) AS avg_latency_ms
         FROM usage u
         GROUP BY COALESCE(u.provider_id, 'unknown')
         ORDER BY requests DESC",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default()
}

/// All-time rollup per model — powers the Dashboard "Top Models" card.
#[derive(Debug, serde::Serialize, sqlx::FromRow)]
pub struct UsagePerModelRow {
    pub model_id: String,
    pub provider_id: String,
    pub requests: i64,
    pub success: i64,
    pub errors: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    pub avg_latency_ms: i64,
}

pub async fn usage_per_model(pool: &PgPool) -> Vec<UsagePerModelRow> {
    sqlx::query_as::<_, UsagePerModelRow>(
        "SELECT
            COALESCE(u.model_id, 'unknown') AS model_id,
            COALESCE(u.provider_id, 'unknown') AS provider_id,
            COUNT(*)::BIGINT AS requests,
            COALESCE(SUM(CASE WHEN u.status = 'error' THEN 0 ELSE 1 END), 0)::BIGINT AS success,
            COALESCE(SUM(CASE WHEN u.status = 'error' THEN 1 ELSE 0 END), 0)::BIGINT AS errors,
            COALESCE(SUM(u.prompt_tokens), 0)::BIGINT AS prompt_tokens,
            COALESCE(SUM(u.completion_tokens), 0)::BIGINT AS completion_tokens,
            COALESCE(SUM(u.total_tokens), 0)::BIGINT AS total_tokens,
            COALESCE(CAST(AVG(NULLIF(u.latency_ms, 0)) AS BIGINT), 0) AS avg_latency_ms
         FROM usage u
         GROUP BY COALESCE(u.model_id, 'unknown'), COALESCE(u.provider_id, 'unknown')
         ORDER BY total_tokens DESC",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default()
}

/// Today's rollup (UTC) — powers the "Today:" lines on the stat cards.
#[derive(Debug, serde::Serialize, sqlx::FromRow)]
pub struct UsageTodayRow {
    pub requests: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
}

pub async fn usage_today(pool: &PgPool) -> UsageTodayRow {
    sqlx::query_as::<_, UsageTodayRow>(
        "SELECT
            COUNT(*)::BIGINT AS requests,
            COALESCE(SUM(prompt_tokens), 0)::BIGINT AS prompt_tokens,
            COALESCE(SUM(completion_tokens), 0)::BIGINT AS completion_tokens,
            COALESCE(SUM(total_tokens), 0)::BIGINT AS total_tokens
         FROM usage
         WHERE (created_at::timestamp)::date = CURRENT_DATE",
    )
    .fetch_one(pool)
    .await
    .unwrap_or(UsageTodayRow {
        requests: 0,
        prompt_tokens: 0,
        completion_tokens: 0,
        total_tokens: 0,
    })
}

/// Overall latency percentiles (all-time) for the Latency card.
#[derive(Debug, serde::Serialize, sqlx::FromRow)]
pub struct UsageLatencyRow {
    pub samples: i64,
    pub avg_ms: i64,
    pub p95_ms: i64,
    pub min_ms: i64,
    pub max_ms: i64,
}

pub async fn usage_latency(pool: &PgPool) -> UsageLatencyRow {
    // SQLite has no PERCENTILE_CONT — derive p95 by offsetting into the sorted set.
    let row: Option<UsageLatencyRow> = sqlx::query_as::<_, UsageLatencyRow>(
        "WITH l AS (
            SELECT latency_ms FROM usage WHERE latency_ms > 0 ORDER BY latency_ms
         )
         SELECT
            (SELECT COUNT(*)::BIGINT FROM l) AS samples,
            COALESCE(CAST((SELECT AVG(latency_ms) FROM l) AS BIGINT), 0) AS avg_ms,
            COALESCE((SELECT latency_ms FROM l LIMIT 1 OFFSET (SELECT GREATEST(0, (COUNT(*) * 95 / 100) - 1) FROM l)), 0)::BIGINT AS p95_ms,
            COALESCE((SELECT MIN(latency_ms) FROM l), 0)::BIGINT AS min_ms,
            COALESCE((SELECT MAX(latency_ms) FROM l), 0)::BIGINT AS max_ms",
    )
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();

    row.unwrap_or(UsageLatencyRow {
        samples: 0,
        avg_ms: 0,
        p95_ms: 0,
        min_ms: 0,
        max_ms: 0,
    })
}

#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct UsageLogRow {
    pub id: String,
    pub created_at: String,
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    /// Serialized as `api_key_id` for FE compatibility (Logs page expects this field).
    #[serde(rename = "api_key_id", alias = "gateway_key_id")]
    pub gateway_key_id: Option<String>,
    pub endpoint: Option<String>,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    pub latency_ms: i64,
    pub ttft_ms: Option<i64>,
    pub status: Option<String>,
    pub status_code: Option<i64>,
    pub error_message: Option<String>,
    pub key_label: Option<String>,
    /// Label of the underlying provider_api_key that handled this request.
    /// `None` when the row has no `provider_api_key_id` (no key was attempted).
    pub provider_key_label: Option<String>,
    pub request_body: Option<String>,
    pub response_body: Option<String>,
}

pub async fn usage_logs_page(
    pool: &PgPool,
    limit: i64,
    offset: i64,
) -> Vec<UsageLogRow> {
    sqlx::query_as::<_, UsageLogRow>(
        "SELECT u.id, u.created_at, u.provider_id, u.model_id, u.gateway_key_id, u.endpoint,
                u.prompt_tokens, u.completion_tokens, u.total_tokens, u.latency_ms, u.ttft_ms,
                u.status, u.status_code, u.error_message,
                u.request_body, u.response_body,
                COALESCE(gk.label, '') AS key_label,
                COALESCE(ak.label, '') AS provider_key_label
         FROM usage u
         LEFT JOIN gateway_keys gk ON gk.id = u.gateway_key_id
         LEFT JOIN api_keys ak ON ak.id = u.provider_api_key_id
         ORDER BY u.created_at DESC, u.created_at DESC
         LIMIT $1 OFFSET $2",
    )
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await
    .unwrap_or_default()
}

pub async fn count_usage_logs(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM usage")
        .fetch_one(pool)
        .await
        .unwrap_or(0)
}

/// Fetch the single most-recently inserted row. Used by `UsageTracker`
/// to broadcast the just-saved row to SSE subscribers.
pub async fn fetch_latest_usage(pool: &PgPool) -> Option<UsageLogRow> {
    sqlx::query_as::<_, UsageLogRow>(
        "SELECT u.id, u.created_at, u.provider_id, u.model_id, u.gateway_key_id, u.endpoint,
                u.prompt_tokens, u.completion_tokens, u.total_tokens, u.latency_ms, u.ttft_ms,
                u.status, u.status_code, u.error_message,
                u.request_body, u.response_body,
                COALESCE(gk.label, '') AS key_label,
                COALESCE(ak.label, '') AS provider_key_label
         FROM usage u
         LEFT JOIN gateway_keys gk ON gk.id = u.gateway_key_id
         LEFT JOIN api_keys ak ON ak.id = u.provider_api_key_id
         ORDER BY u.created_at DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
}

/// Build the broadcast payload from the entry just written.
///
/// The old path did a `SELECT … ORDER BY created_at DESC LIMIT 1` after every INSERT to
/// get a row for the SSE broadcast. That read is unnecessary: the entry already holds
/// every field the row carries, so one request costs one INSERT instead of two queries.
///
/// `key_label` / `provider_key_label` are the only fields the database could join in and
/// the entry does not carry. They stay empty — the live usage stream is a latency signal
/// for the admin dashboard, not an audit trail, and the Logs page still reads them from
/// the database where the join belongs.
pub fn usage_row_from_entry(e: &UsageEntry) -> UsageLogRow {
    UsageLogRow {
        id: format!("usage_{}", uuid::Uuid::new_v4().simple()),
        created_at: chrono::Utc::now().format("%Y-%m-%d %H:%M:%S%.6f %:z").to_string(),
        provider_id: Some(e.provider_id.clone()),
        model_id: Some(e.model_id.clone()),
        gateway_key_id: Some(e.gateway_key_id.clone()),
        endpoint: Some(e.endpoint.clone()),
        prompt_tokens: e.prompt_tokens,
        completion_tokens: e.completion_tokens,
        total_tokens: e.prompt_tokens + e.completion_tokens,
        latency_ms: e.latency_ms,
        ttft_ms: e.ttft_ms,
        status: Some(e.status.clone()),
        status_code: Some(e.status_code),
        error_message: e.error_message.clone(),
        key_label: None,
        provider_key_label: None,
        request_body: None,
        response_body: None,
    }
}

/// Check if a model is blocked for a provider.
pub async fn is_model_blocked(pool: &PgPool, provider_id: &str, model_id: &str) -> bool {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT 1 FROM blocked_models WHERE provider_id = $1 AND model_id = $2"
    )
    .bind(provider_id)
    .bind(model_id)
    .fetch_optional(pool)
    .await
    .unwrap_or(None);
    row.is_some()
}

/// Check if a model is globally disabled (outer middleware).
pub async fn is_model_disabled(pool: &PgPool, model_id: &str) -> bool {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT model_id FROM disabled_models WHERE model_id = $1"
    )
    .bind(model_id)
    .fetch_optional(pool)
    .await
    .unwrap_or(None);
    row.is_some()
}

/// Block a model for a provider. Returns true if inserted (true if inserted—false when already existed) false=was already blocking
pub async fn block_model(pool: &PgPool, provider_id: &str, model_id: &str) -> anyhow::Result<bool> {
    let id = format!("{}/{}", provider_id, model_id);
    let result = sqlx::query(
        "INSERT INTO blocked_models (id, provider_id, model_id) VALUES ($1, $2, $3)"
    )
    .bind(&id)
    .bind(provider_id)
    .bind(model_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Unblock a model for a provider.
pub async fn unblock_model(pool: &PgPool, provider_id: &str, model_id: &str) -> anyhow::Result<bool> {
    let result = sqlx::query("DELETE FROM blocked_models WHERE provider_id = $1 AND model_id = $2")
        .bind(provider_id)
        .bind(model_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

// ── Custom providers ──

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CustomProviderRow {
    pub id: String,
    pub name: String,
    pub prefix: String,
    pub base_url: String,
    pub validate_url: String,
    pub color: String,
    pub timeout_secs: i64,
    pub first_chunk_timeout_secs: i64,
    pub stall_timeout_secs: i64,
    pub created_at: String,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CustomProviderModelRow {
    pub provider_id: String,
    pub model_id: String,
    pub ctx: i64,
    pub vision: i64,
    pub tools: i64,
}

pub async fn list_custom_providers(pool: &PgPool) -> anyhow::Result<Vec<CustomProviderRow>> {
    Ok(sqlx::query_as::<_, CustomProviderRow>(
        "SELECT * FROM custom_providers ORDER BY created_at"
    ).fetch_all(pool).await?)
}

pub async fn get_custom_provider(pool: &PgPool, id: &str) -> anyhow::Result<Option<CustomProviderRow>> {
    Ok(sqlx::query_as::<_, CustomProviderRow>(
        "SELECT * FROM custom_providers WHERE id = $1"
    ).bind(id).fetch_optional(pool).await?)
}

pub async fn list_custom_provider_models(pool: &PgPool, provider_id: &str) -> anyhow::Result<Vec<CustomProviderModelRow>> {
    Ok(sqlx::query_as::<_, CustomProviderModelRow>(
        "SELECT * FROM custom_provider_models WHERE provider_id = $1 ORDER BY model_id"
    ).bind(provider_id).fetch_all(pool).await?)
}

pub async fn create_custom_provider(
    pool: &PgPool, id: &str, name: &str, prefix: &str,
    base_url: &str, validate_url: &str, color: &str,
    timeout: i64, fc_timeout: i64, stall_timeout: i64,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO custom_providers (id, name, prefix, base_url, validate_url, color, timeout_secs, first_chunk_timeout_secs, stall_timeout_secs) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)"
    ).bind(id).bind(name).bind(prefix).bind(base_url).bind(validate_url).bind(color).bind(timeout).bind(fc_timeout).bind(stall_timeout)
    .execute(pool).await?;
    Ok(())
}

pub async fn delete_custom_provider(pool: &PgPool, id: &str) -> anyhow::Result<bool> {
    // Root provider row
    let r = sqlx::query("DELETE FROM custom_providers WHERE id = $1").bind(id).execute(pool).await?;
    if r.rows_affected() > 0 {
        // Cascade — API keys, model definitions are orphaned otherwise
        let _ = sqlx::query("DELETE FROM api_keys WHERE provider_id = $1").bind(id).execute(pool).await;
        let _ = sqlx::query("DELETE FROM custom_provider_models WHERE provider_id = $1").bind(id).execute(pool).await;
    }
    Ok(r.rows_affected() > 0)
}

pub async fn add_custom_provider_model(pool: &PgPool, provider_id: &str, model_id: &str, ctx: i64, vision: i64, tools: i64) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO custom_provider_models (provider_id, model_id, ctx, vision, tools) VALUES ($1, $2, $3, $4, $5) ON CONFLICT (provider_id, model_id) DO UPDATE SET ctx = EXCLUDED.ctx, vision = EXCLUDED.vision, tools = EXCLUDED.tools"
    ).bind(provider_id).bind(model_id).bind(ctx).bind(vision).bind(tools)
    .execute(pool).await?;
    Ok(())
}

pub async fn remove_custom_provider_model(pool: &PgPool, provider_id: &str, model_id: &str) -> anyhow::Result<bool> {
    let r = sqlx::query(
        "DELETE FROM custom_provider_models WHERE provider_id = $1 AND model_id = $2"
    ).bind(provider_id).bind(model_id).execute(pool).await?;
    Ok(r.rows_affected() > 0)
}

// ── Custom Models (per-provider user-added model entries) ──

#[derive(Debug, serde::Serialize, sqlx::FromRow)]
pub struct CustomModelRow {
    pub id: String,
    pub provider_id: String,
    pub model_id: String,
    pub display_name: String,
    pub ctx: i64,
    pub vision: i64,
    pub tools: i64,
    pub created_at: String,
}

pub async fn list_custom_models(pool: &PgPool, provider_id: &str) -> Vec<CustomModelRow> {
    sqlx::query_as::<_, CustomModelRow>(
        "SELECT * FROM custom_models WHERE provider_id = $1 ORDER BY model_id"
    )
    .bind(provider_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default()
}

pub async fn add_custom_model(
    pool: &PgPool,
    provider_id: &str,
    model_id: &str,
    display_name: &str,
    ctx: i64,
    vision: i64,
    tools: i64,
) -> anyhow::Result<bool> {
    let id = format!("{}_{}", provider_id, model_id.replace('/', "_"));
    let r = sqlx::query(
        "INSERT INTO custom_models (id, provider_id, model_id, display_name, ctx, vision, tools) VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT (provider_id, model_id) DO UPDATE SET display_name = EXCLUDED.display_name, ctx = EXCLUDED.ctx, vision = EXCLUDED.vision, tools = EXCLUDED.tools"
    )
    .bind(&id)
    .bind(provider_id)
    .bind(model_id)
    .bind(display_name)
    .bind(ctx)
    .bind(vision)
    .bind(tools)
    .execute(pool)
    .await?;
    Ok(r.rows_affected() > 0)
}

pub async fn remove_custom_model(pool: &PgPool, provider_id: &str, model_id: &str) -> anyhow::Result<bool> {
    let r = sqlx::query(
        "DELETE FROM custom_models WHERE provider_id = $1 AND model_id = $2"
    )
    .bind(provider_id)
    .bind(model_id)
    .execute(pool)
    .await?;
    Ok(r.rows_affected() > 0)
}

// ===== Unsloth models — self-contained routing (base_url + api_key + upstream_model per row) =====

#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct UnslothModelRow {
    pub id: String,
    pub label: String,
    pub base_url: String,
    pub api_key: String,
    pub upstream_model: String,
    pub context_length: i64,
    pub supports_tools: i64,
    pub supports_vision: i64,
    pub is_active: i64,
    pub created_at: String,
    pub updated_at: String,
}

pub async fn list_unsloth_models(pool: &PgPool) -> anyhow::Result<Vec<UnslothModelRow>> {
    let rows = sqlx::query_as::<_, UnslothModelRow>(
        r#"SELECT id, label, base_url, api_key, upstream_model,
            context_length, supports_tools,
            supports_vision,
            is_active,
            created_at, updated_at
            FROM unsloth_models ORDER BY created_at DESC"#
    ).fetch_all(pool).await?;
    Ok(rows)
}

pub async fn get_unsloth_model(pool: &PgPool, id: &str) -> anyhow::Result<Option<UnslothModelRow>> {
    let row = sqlx::query_as::<_, UnslothModelRow>(
        r#"SELECT id, label, base_url, api_key, upstream_model,
            context_length, supports_tools,
            supports_vision,
            is_active,
            created_at, updated_at
            FROM unsloth_models WHERE id = $1"#,
    )
    .bind(id)
    .fetch_optional(pool).await?;
    Ok(row)
}

pub async fn create_unsloth_model(
    pool: &PgPool,
    id: &str, label: &str, base_url: &str, api_key: &str,
    upstream_model: &str, context_length: i64,
    supports_tools: bool, supports_vision: bool,
) -> anyhow::Result<()> {
    sqlx::query(
        r#"INSERT INTO unsloth_models (id, label, base_url, api_key, upstream_model,
            context_length, supports_tools, supports_vision, is_active)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 1)"#
    )
    .bind(id).bind(label).bind(base_url).bind(api_key)
    .bind(upstream_model).bind(context_length)
    .bind(supports_tools).bind(supports_vision)
    .execute(pool).await?;
    Ok(())
}

pub async fn update_unsloth_model(
    pool: &PgPool,
    id: &str, label: &str, base_url: &str, api_key: &str,
    upstream_model: &str, context_length: i64,
    supports_tools: bool, supports_vision: bool, is_active: bool,
) -> anyhow::Result<bool> {
    let r = sqlx::query(
        r#"UPDATE unsloth_models SET
            label = $2, base_url = $3, api_key = $4, upstream_model = $5,
            context_length = $6, supports_tools = $7, supports_vision = $8,
            is_active = $9, updated_at = NOW()
            WHERE id = $1"#
    )
    .bind(id).bind(label).bind(base_url).bind(api_key)
    .bind(upstream_model).bind(context_length)
    .bind(supports_tools).bind(supports_vision)
    .bind(is_active)
    .execute(pool).await?;
    Ok(r.rows_affected() > 0)
}

pub async fn delete_unsloth_model(pool: &PgPool, id: &str) -> anyhow::Result<bool> {
    let r = sqlx::query("DELETE FROM unsloth_models WHERE id = $1")
        .bind(id)
        .execute(pool).await?;
    Ok(r.rows_affected() > 0)
}

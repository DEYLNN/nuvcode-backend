use sqlx::PgPool;

pub async fn run(pool: &PgPool) -> anyhow::Result<()> {
    // ── Core tables ──────────────────────────────────────────────

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS api_keys (
            id TEXT PRIMARY KEY,
            provider_id TEXT NOT NULL,
            key_value TEXT NOT NULL,
            label TEXT,
            is_active BOOLEAN NOT NULL DEFAULT TRUE,
            rate_limit BIGINT,
            last_used_at TEXT,
            consecutive_use_count BIGINT NOT NULL DEFAULT 0,
            consecutive_error_count BIGINT NOT NULL DEFAULT 0,
            locked_until TEXT,
            last_error_status INTEGER,
            last_error_message TEXT,
            last_error_at TEXT,
            backoff_level BIGINT NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL DEFAULT TO_CHAR(NOW(), 'YYYY-MM-DD"T"HH24:MI:SS'),
            updated_at TEXT NOT NULL DEFAULT TO_CHAR(NOW(), 'YYYY-MM-DD"T"HH24:MI:SS'),
            key_type TEXT
        )
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_api_keys_provider ON api_keys(provider_id, is_active)")
        .execute(pool)
        .await?;

    // Gateway API keys (client-facing auth for /v1/*)
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS gateway_keys (
            id TEXT PRIMARY KEY,
            key_value TEXT NOT NULL UNIQUE,
            label TEXT,
            is_active BOOLEAN NOT NULL DEFAULT TRUE,
            access_type TEXT NOT NULL DEFAULT 'full',
            allowed_models TEXT NOT NULL DEFAULT '',
            max_tokens BIGINT NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL DEFAULT TO_CHAR(NOW(), 'YYYY-MM-DD"T"HH24:MI:SS')
        )
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_gateway_keys_active ON gateway_keys(is_active)")
        .execute(pool)
        .await?;

    // Blocked models — models admin has disabled per-provider
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS blocked_models (
            id TEXT PRIMARY KEY,
            provider_id TEXT NOT NULL,
            model_id TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT TO_CHAR(NOW(), 'YYYY-MM-DD"T"HH24:MI:SS'),
            UNIQUE(provider_id, model_id)
        )
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_blocked_models_provider ON blocked_models(provider_id)")
        .execute(pool)
        .await?;

    // Disabled models — global model allowlist (outer middleware)
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS disabled_models (
            model_id TEXT PRIMARY KEY,
            created_at TEXT NOT NULL DEFAULT TO_CHAR(NOW(), 'YYYY-MM-DD"T"HH24:MI:SS')
        )
        "#,
    )
    .execute(pool)
    .await?;

    // Settings table
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS settings (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL DEFAULT '',
            updated_at TEXT NOT NULL DEFAULT TO_CHAR(NOW(), 'YYYY-MM-DD"T"HH24:MI:SS')
        )
        "#,
    )
    .execute(pool)
    .await?;

    // Default settings
    sqlx::query("INSERT INTO settings (key, value) VALUES ('rtk_enabled', 'true') ON CONFLICT (key) DO NOTHING")
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO settings (key, value) VALUES ('caveman_enabled', 'false') ON CONFLICT (key) DO NOTHING")
        .execute(pool)
        .await?;

    // Custom OpenAI-compatible providers
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS custom_providers (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            prefix TEXT NOT NULL,
            base_url TEXT NOT NULL,
            validate_url TEXT NOT NULL DEFAULT '',
            color TEXT NOT NULL DEFAULT '#6366F1',
            timeout_secs BIGINT NOT NULL DEFAULT 120,
            first_chunk_timeout_secs BIGINT NOT NULL DEFAULT 200,
            stall_timeout_secs BIGINT NOT NULL DEFAULT 360,
            created_at TEXT NOT NULL DEFAULT TO_CHAR(NOW(), 'YYYY-MM-DD"T"HH24:MI:SS'),
            updated_at TEXT NOT NULL DEFAULT TO_CHAR(NOW(), 'YYYY-MM-DD"T"HH24:MI:SS')
        )
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS custom_provider_models (
            provider_id TEXT NOT NULL REFERENCES custom_providers(id) ON DELETE CASCADE,
            model_id TEXT NOT NULL,
            ctx BIGINT NOT NULL DEFAULT 4096,
            vision BIGINT NOT NULL DEFAULT 0,
            tools BIGINT NOT NULL DEFAULT 1,
            UNIQUE(provider_id, model_id)
        )
        "#,
    )
    .execute(pool)
    .await?;

    // Usage table — per-request token tracking
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS usage (
            id TEXT PRIMARY KEY,
            provider_id TEXT NOT NULL,
            model_id TEXT NOT NULL,
            gateway_key_id TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'success',
            status_code BIGINT DEFAULT 200,
            prompt_tokens BIGINT NOT NULL DEFAULT 0,
            completion_tokens BIGINT NOT NULL DEFAULT 0,
            total_tokens BIGINT NOT NULL DEFAULT 0,
            latency_ms BIGINT NOT NULL DEFAULT 0,
            endpoint TEXT,
            error_message TEXT,
            provider_api_key_id TEXT,
            ttft_ms BIGINT,
            request_body TEXT,
            response_body TEXT,
            created_at TEXT NOT NULL DEFAULT TO_CHAR(NOW(), 'YYYY-MM-DD"T"HH24:MI:SS')
        )
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_usage_key ON usage(gateway_key_id)")
        .execute(pool)
        .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_usage_provider ON usage(provider_id)")
        .execute(pool)
        .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_usage_model ON usage(model_id)")
        .execute(pool)
        .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_usage_created ON usage(created_at DESC)")
        .execute(pool)
        .await?;

    // Custom models — per-provider user-added model entries
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS custom_models (
            id TEXT PRIMARY KEY,
            provider_id TEXT NOT NULL,
            model_id TEXT NOT NULL,
            display_name TEXT NOT NULL DEFAULT '',
            ctx BIGINT NOT NULL DEFAULT 4096,
            vision BIGINT NOT NULL DEFAULT 0,
            tools BIGINT NOT NULL DEFAULT 1,
            created_at TEXT NOT NULL DEFAULT TO_CHAR(NOW(), 'YYYY-MM-DD"T"HH24:MI:SS'),
            UNIQUE(provider_id, model_id)
        )
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_custom_models_provider ON custom_models(provider_id)")
        .execute(pool)
        .await?;

    // Combos — virtual model alias
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS combos (
            id TEXT PRIMARY KEY,
            name TEXT UNIQUE NOT NULL,
            strategy TEXT NOT NULL DEFAULT 'fallback',
            tiers TEXT NOT NULL DEFAULT '[]',
            is_active INTEGER NOT NULL DEFAULT 1,
            min_context BIGINT NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL DEFAULT TO_CHAR(NOW(), 'YYYY-MM-DD"T"HH24:MI:SS'),
            updated_at TEXT NOT NULL DEFAULT TO_CHAR(NOW(), 'YYYY-MM-DD"T"HH24:MI:SS')
        )
        "#,
    )
    .execute(pool)
    .await?;

    // Unsloth models — per-model self-contained routing
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS unsloth_models (
            id TEXT PRIMARY KEY,
            label TEXT NOT NULL,
            base_url TEXT NOT NULL,
            api_key TEXT NOT NULL,
            upstream_model TEXT NOT NULL,
            context_length BIGINT NOT NULL DEFAULT 128000,
            supports_tools BIGINT NOT NULL DEFAULT 0,
            supports_vision BIGINT NOT NULL DEFAULT 0,
            is_active INTEGER NOT NULL DEFAULT 1,
            created_at TEXT NOT NULL DEFAULT TO_CHAR(NOW(), 'YYYY-MM-DD"T"HH24:MI:SS'),
            updated_at TEXT NOT NULL DEFAULT TO_CHAR(NOW(), 'YYYY-MM-DD"T"HH24:MI:SS')
        )
        "#,
    )
    .execute(pool)
    .await?;

    tracing::info!("Database migrations complete");
    Ok(())
}

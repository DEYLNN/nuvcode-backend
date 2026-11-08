use std::sync::Arc;

use axum::{extract::State, Json};

use crate::state::AppState;

#[derive(serde::Serialize)]
pub struct ApiSettingsResponse {
    pub rtk_enabled: String,
    pub caveman_enabled: String,
    pub ponytail_enabled: String,
    pub gateway_timeout: i64,
    pub public_ip: String,
    pub public_url: String,
    pub port: u16,
    pub server_host: String,
    pub api_key_header: String,
    pub api_key_prefix: String,
    pub database_url: String,
    pub keys_count: i64,
    pub in_flight: i64,
}

pub async fn api_settings(State(state): State<Arc<AppState>>) -> Json<ApiSettingsResponse> {
    let keys_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM api_keys")
        .fetch_one(&state.db).await.unwrap_or(0);
    let cfg = &state.config;

    // Load actual settings from DB
    let rtk_enabled: String = sqlx::query_scalar("SELECT value FROM settings WHERE key='rtk_enabled'")
        .fetch_optional(&state.db).await.unwrap_or(None).unwrap_or_else(|| "off".into());
    let caveman_enabled: String = sqlx::query_scalar("SELECT value FROM settings WHERE key='caveman_enabled'")
        .fetch_optional(&state.db).await.unwrap_or(None).unwrap_or_else(|| "off".into());
    let ponytail_enabled: String = sqlx::query_scalar("SELECT value FROM settings WHERE key='ponytail_enabled'")
        .fetch_optional(&state.db).await.unwrap_or(None).unwrap_or_else(|| "off".into());

    Json(ApiSettingsResponse {
        rtk_enabled,
        caveman_enabled,
        ponytail_enabled,
        gateway_timeout: cfg.gateway.timeout_secs as i64,
        public_ip: state.public_ip.clone(),
        public_url: state.public_url.clone(),
        port: cfg.server.port,
        server_host: cfg.server.host.clone(),
        api_key_header: cfg.auth.api_key_header.clone(),
        api_key_prefix: cfg.auth.api_key_prefix.clone(),
        database_url: cfg.database.url.clone(),
        keys_count,
        in_flight: state.in_flight.load(std::sync::atomic::Ordering::Relaxed),
    })
}

#[derive(serde::Deserialize)]
pub struct ToggleRequest {
    pub key: String,
    pub value: String,
}

pub async fn api_toggle_setting(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ToggleRequest>,
) -> Json<serde_json::Value> {
    // Simpan ke DB — upsert
    let result = sqlx::query(
        "INSERT INTO settings (key, value, updated_at) VALUES ($1, $2, NOW())
         ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at"
    )
    .bind(&req.key)
    .bind(&req.value)
    .execute(&state.db)
    .await;

    match result {
        Ok(_) => Json(serde_json::json!({"ok": true})),
        Err(e) => Json(serde_json::json!({"ok": false, "error": e.to_string()})),
    }
}
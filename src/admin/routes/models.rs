use std::sync::Arc;

use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};

use crate::state::AppState;

#[derive(Deserialize)]
pub struct ToggleModelRequest {
    pub model_id: String,
    pub enabled: bool,
}

#[derive(Serialize)]
pub struct ToggleModelResponse {
    pub ok: bool,
    pub model_id: String,
    pub enabled: bool,
}

/// Toggle a model's enabled/disabled state (global allowlist)
pub async fn api_toggle_model(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ToggleModelRequest>,
) -> Json<ToggleModelResponse> {
    if req.enabled {
        // Enable: remove from disabled list
        let _ = sqlx::query("DELETE FROM disabled_models WHERE model_id = $1")
            .bind(&req.model_id)
            .execute(&state.db)
            .await;
    } else {
        // Disable: add to disabled list
        let _ = sqlx::query("INSERT INTO disabled_models (model_id) VALUES ($1)")
            .bind(&req.model_id)
            .execute(&state.db)
            .await;
    }
    Json(ToggleModelResponse {
        ok: true,
        model_id: req.model_id,
        enabled: req.enabled,
    })
}

/// Get all disabled model IDs
pub async fn api_disabled_models(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<String>> {
    let rows: Vec<String> = sqlx::query_scalar("SELECT model_id FROM disabled_models")
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();
    Json(rows)
}

/// Get all models grouped by provider with enabled/disabled status
pub async fn api_all_models(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let pm = state.provider_manager.read().await;
    let all_models = pm.list_all_models().await;

    let disabled: std::collections::HashSet<String> = sqlx::query_scalar("SELECT model_id FROM disabled_models")
        .fetch_all(&state.db)
        .await
        .unwrap_or_default()
        .into_iter()
        .collect();

    let mut providers_map: std::collections::BTreeMap<String, Vec<serde_json::Value>> = std::collections::BTreeMap::new();
    for m in &all_models {
        let parts: Vec<&str> = m.id.split('/').collect();
        let provider = parts.first().unwrap_or(&"unknown").to_string();
        let is_disabled = disabled.contains(&m.id);
        providers_map.entry(provider).or_default().push(serde_json::json!({
            "id": m.id,
            "owned_by": m.owned_by,
            "enabled": !is_disabled,
            "context_length": m.context_length,
        }));
    }

    // Append active combos as a virtual "combo" provider group so the admin
    // Models page can show them alongside real providers, with enable/disable
    // and context length — same toggle UX as real models.
    {
        let combo_rows: Vec<(String, String, i64, i64)> = sqlx::query_as(
            "SELECT id, name, min_context, is_active::bigint FROM combos ORDER BY name"
        )
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();
        if !combo_rows.is_empty() {
            let combo_entry = combo_rows.into_iter().map(|(_id, name, min_ctx, is_active)| {
                let model_id = format!("combo/{}", name);
                let is_disabled = disabled.contains(&model_id);
                serde_json::json!({
                    "id": model_id,
                    "owned_by": "combo",
                    "enabled": !is_disabled,
                    "context_length": if min_ctx > 0 { Some(min_ctx as u32) } else { None },
                    "combo_active": is_active != 0,
                })
            }).collect::<Vec<_>>();
            providers_map.insert("combo".to_string(), combo_entry);
        }
    }

    Json(serde_json::json!(providers_map))
}

/// Get all blocked models from the per-provider blocked_models table
pub async fn api_blocked_models(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<serde_json::Value>> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT provider_id, model_id FROM blocked_models"
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|(provider_id, model_id): (String, String)| {
        (provider_id, model_id)
    })
    .collect();
    Json(rows.into_iter().map(|(p, m)| serde_json::json!({ "provider": p, "model": m })).collect())
}

/// Bulk enable or disable all models for a provider in one call.
/// Used by ModelsSection "Enable all" / "Disable all" buttons.
#[derive(Deserialize)]
pub struct BulkToggleRequest {
    pub provider_id: String,
    pub enabled: bool,
}

#[derive(Serialize)]
pub struct BulkToggleResponse {
    pub ok: bool,
    pub provider_id: String,
    pub enabled: bool,
    pub affected: usize,
}

pub async fn api_bulk_toggle(
    State(state): State<Arc<AppState>>,
    Json(req): Json<BulkToggleRequest>,
) -> Json<BulkToggleResponse> {
    let models = state
        .provider_manager
        .read()
        .await
        .list_all_models_unfiltered()
        .await;
    let prefix = format!("{}/", req.provider_id);
    let mut affected = 0usize;
    for m in &models {
        if !m.id.starts_with(&prefix) {
            continue;
        }
        let r = if req.enabled {
            // Enable: remove from disabled list
            sqlx::query("DELETE FROM disabled_models WHERE model_id = $1")
                .bind(&m.id)
                .execute(&state.db)
                .await
        } else {
            // Disable: add to disabled list
            sqlx::query("INSERT INTO disabled_models (model_id) VALUES ($1)")
                .bind(&m.id)
                .execute(&state.db)
                .await
        };
        if r.is_ok() {
            affected += 1;
        }
    }
    Json(BulkToggleResponse {
        ok: true,
        provider_id: req.provider_id,
        enabled: req.enabled,
        affected,
    })
}

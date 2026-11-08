use std::sync::Arc;

use axum::{
    extract::{Path, State},
    Json,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::state::AppState;

#[derive(Deserialize)]
pub struct CreateComboRequest {
    pub name: String,
    pub strategy: Option<String>, // "fallback" | "round_robin"
    pub tiers: Vec<String>,      // ["sop/deepseek-v4-pro", "verb/deepseek-v4-flash-0731"]
}

#[derive(Serialize)]
pub struct ComboResponse {
    pub id: String,
    pub name: String,
    pub strategy: String,
    pub tiers: Vec<String>,
    /// `is_active` is a BIGINT column (0/1) in Postgres. Serialized as a JSON
    /// boolean because the FE reads it as `combo.is_active` in a boolean context.
    pub is_active: bool,
    pub min_context: i64,
    pub created_at: String,
    pub updated_at: String,
}

/// List all combos
pub async fn api_list_combos(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<ComboResponse>> {
    let rows = sqlx::query_as::<_, (String, String, String, String, i64, i64, String, String)>(
        "SELECT id, name, strategy, tiers, is_active, min_context, created_at, updated_at FROM combos ORDER BY created_at DESC"
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|(id, name, strategy, tiers_str, active, min_ctx, created, updated)| {
        let tiers: Vec<String> = serde_json::from_str(&tiers_str).unwrap_or_default();
        ComboResponse {
            id, name, strategy, tiers,
            is_active: active != 0, min_context: min_ctx,
            created_at: created, updated_at: updated,
        }
    })
    .collect();
    Json(rows)
}

/// Create combo
pub async fn api_create_combo(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateComboRequest>,
) -> Json<serde_json::Value> {
    let id = format!("combo_{}", &Uuid::new_v4().to_string()[..8]);
    let tiers_json = serde_json::to_string(&req.tiers).unwrap_or_else(|_| "[]".into());
    let strategy = req.strategy.unwrap_or_else(|| "fallback".into());

    // Calculate min_context from all tier models
    let pm = state.provider_manager.read().await;
    let mut min_ctx = i64::MAX;
    for tier_model in &req.tiers {
        let all = pm.list_all_models_unfiltered().await;
        if let Some(m) = all.iter().find(|m| m.id == *tier_model) {
            if let Some(ctx) = m.context_length {
                min_ctx = min_ctx.min(ctx as i64);
            }
        }
    }
    drop(pm);
    if min_ctx == i64::MAX { min_ctx = 0; }

    let result = sqlx::query(
        "INSERT INTO combos (id, name, strategy, tiers, min_context) VALUES ($1, $2, $3, $4, $5)"
    )
    .bind(&id)
    .bind(&req.name)
    .bind(&strategy)
    .bind(&tiers_json)
    .bind(min_ctx)
    .execute(&state.db)
    .await;

    match result {
        Ok(_) => Json(serde_json::json!({"ok": true, "id": id, "name": req.name, "min_context": min_ctx})),
        Err(e) => {
            let msg = if e.to_string().contains("UNIQUE") {
                format!("Combo '{}' already exists", req.name)
            } else {
                format!("Failed: {}", e)
            };
            Json(serde_json::json!({"ok": false, "error": msg}))
        }
    }
}

/// Update combo
pub async fn api_update_combo(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<CreateComboRequest>,
) -> Json<serde_json::Value> {
    let tiers_json = serde_json::to_string(&req.tiers).unwrap_or_else(|_| "[]".into());
    let strategy = req.strategy.unwrap_or_else(|| "fallback".into());

    let pm = state.provider_manager.read().await;
    let mut min_ctx = i64::MAX;
    for tier_model in &req.tiers {
        let all = pm.list_all_models_unfiltered().await;
        if let Some(m) = all.iter().find(|m| m.id == *tier_model) {
            if let Some(ctx) = m.context_length {
                min_ctx = min_ctx.min(ctx as i64);
            }
        }
    }
    drop(pm);
    if min_ctx == i64::MAX { min_ctx = 0; }

    let result = sqlx::query(
        "UPDATE combos SET name = $1, strategy = $2, tiers = $3, min_context = $4, updated_at = NOW() WHERE id = $5"
    )
    .bind(&req.name)
    .bind(&strategy)
    .bind(&tiers_json)
    .bind(min_ctx)
    .bind(&id)
    .execute(&state.db)
    .await;

    match result {
        Ok(_) => Json(serde_json::json!({"ok": true, "id": id, "name": req.name, "min_context": min_ctx})),
        Err(e) => Json(serde_json::json!({"ok": false, "error": e.to_string()})),
    }
}

/// Delete combo
pub async fn api_delete_combo(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let result = sqlx::query("DELETE FROM combos WHERE id = $1")
        .bind(&id)
        .execute(&state.db)
        .await;
    match result {
        Ok(_) => Json(serde_json::json!({"ok": true})),
        Err(e) => Json(serde_json::json!({"ok": false, "error": e.to_string()})),
    }
}

/// Toggle combo is_active
pub async fn api_toggle_combo(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let _ = sqlx::query(
        "UPDATE combos SET is_active = CASE WHEN is_active = true THEN 0 ELSE 1 END, updated_at = NOW() WHERE id = $1"
    )
    .bind(&id)
    .execute(&state.db)
    .await;
    Json(serde_json::json!({"ok": true}))
}

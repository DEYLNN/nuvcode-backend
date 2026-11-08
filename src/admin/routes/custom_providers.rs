use std::sync::Arc;

use axum::{extract::{Path, State}, Json};
use serde::{Deserialize, Serialize};

use crate::state::AppState;

#[derive(Deserialize)]
pub struct CreateCustomProviderReq {
    pub id: String,
    pub name: String,
    pub prefix: String,
    pub base_url: String,
    pub validate_url: Option<String>,
    pub color: Option<String>,
    pub timeout_secs: Option<i64>,
    pub first_chunk_timeout_secs: Option<i64>,
    pub stall_timeout_secs: Option<i64>,
    pub models: Option<Vec<CustomModelReq>>,
}

#[derive(Deserialize)]
pub struct CustomModelReq {
    pub model_id: String,
    pub ctx: Option<i64>,
    pub vision: Option<bool>,
    pub tools: Option<bool>,
}

#[derive(Serialize)]
pub struct CustomProviderResp {
    pub id: String,
    pub name: String,
    pub prefix: String,
    pub base_url: String,
    pub validate_url: String,
    pub color: String,
    pub timeout_secs: i64,
    pub first_chunk_timeout_secs: i64,
    pub stall_timeout_secs: i64,
    pub total_keys: i64,
    pub active_keys: i64,
    pub models: Vec<CustomModelResp>,
    pub created_at: String,
}

#[derive(Serialize)]
pub struct CustomModelResp {
    pub id: String,
    pub ctx: i64,
    pub vision: bool,
    pub tools: bool,
}

pub async fn api_create_custom_provider(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateCustomProviderReq>,
) -> Json<serde_json::Value> {
    let validate_url = req.validate_url.unwrap_or_default();
    let color = req.color.unwrap_or_else(|| "#6366F1".to_string());
    let base_url = req.base_url.trim_end_matches('/').to_string();

    if let Err(e) = crate::db::create_custom_provider(
        &state.db, &req.id, &req.name, &req.prefix,
        &base_url, &validate_url, &color,
        req.timeout_secs.unwrap_or(120),
        req.first_chunk_timeout_secs.unwrap_or(200),
        req.stall_timeout_secs.unwrap_or(360),
    ).await {
        let msg = e.to_string();
        let friendly = if msg.contains("UNIQUE") { format!("Custom provider '{}' already exists", req.id) } else { msg };
        return Json(serde_json::json!({"ok": false, "error": friendly}));
    }

    if let Some(models) = &req.models {
        for m in models {
            let _ = crate::db::add_custom_provider_model(
                &state.db, &req.id, &m.model_id,
                m.ctx.unwrap_or(4096),
                if m.vision.unwrap_or(false) { 1 } else { 0 },
                if m.tools.unwrap_or(true) { 1 } else { 0 },
            ).await;
        }
    }

    let mut pm = state.provider_manager.write().await;
    let _ = pm.reload_custom_provider(&req.id, &state.db).await;

    Json(serde_json::json!({"ok": true, "id": req.id}))
}

pub async fn api_list_custom_providers(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<serde_json::Value>> {
    let rows = crate::db::list_custom_providers(&state.db).await.unwrap_or_default();
    let mut out = Vec::new();
    for r in &rows {
        let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM api_keys WHERE provider_id=$1")
            .bind(&r.id).fetch_one(&state.db).await.unwrap_or(0);
        let active: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM api_keys WHERE provider_id=$1 AND is_active=true")
            .bind(&r.id).fetch_one(&state.db).await.unwrap_or(0);
        let models = crate::db::list_custom_provider_models(&state.db, &r.id).await.unwrap_or_default();
        let mlist: Vec<serde_json::Value> = models.iter().map(|m| {
            serde_json::json!({
                "id": m.model_id, "ctx": m.ctx, "vision": m.vision != 0, "tools": m.tools != 0,
            })
        }).collect();
        out.push(serde_json::json!({
            "id": r.id, "name": r.name, "prefix": r.prefix,
            "base_url": r.base_url, "validate_url": r.validate_url,
            "color": r.color, "total_keys": total, "active_keys": active,
            "timeout_secs": r.timeout_secs,
            "first_chunk_timeout_secs": r.first_chunk_timeout_secs,
            "stall_timeout_secs": r.stall_timeout_secs,
            "models": mlist, "created_at": r.created_at,
        }));
    }
    Json(out)
}

pub async fn api_get_custom_provider(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let row = match crate::db::get_custom_provider(&state.db, &id).await.unwrap_or(None) {
        Some(r) => r,
        None => return Json(serde_json::json!({"error": "Not found"})),
    };
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM api_keys WHERE provider_id=$1")
        .bind(&id).fetch_one(&state.db).await.unwrap_or(0);
    let active: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM api_keys WHERE provider_id=$1 AND is_active=true")
        .bind(&id).fetch_one(&state.db).await.unwrap_or(0);
    let models = crate::db::list_custom_provider_models(&state.db, &id).await.unwrap_or_default();
    let mlist: Vec<serde_json::Value> = models.iter().map(|m| {
        serde_json::json!({
            "id": m.model_id, "ctx": m.ctx, "vision": m.vision != 0, "tools": m.tools != 0,
        })
    }).collect();
    Json(serde_json::json!({
        "id": row.id, "name": row.name, "prefix": row.prefix,
        "base_url": row.base_url, "validate_url": row.validate_url,
        "color": row.color, "total_keys": total, "active_keys": active,
        "timeout_secs": row.timeout_secs,
        "first_chunk_timeout_secs": row.first_chunk_timeout_secs,
        "stall_timeout_secs": row.stall_timeout_secs,
        "models": mlist, "created_at": row.created_at,
    }))
}

#[derive(Deserialize)]
pub struct AddModelReq {
    pub model_id: String,
    pub ctx: Option<i64>,
    pub vision: Option<bool>,
    pub tools: Option<bool>,
}

pub async fn api_add_custom_model(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<AddModelReq>,
) -> Json<serde_json::Value> {
    if let Err(e) = crate::db::add_custom_provider_model(
        &state.db, &id, &req.model_id,
        req.ctx.unwrap_or(4096),
        if req.vision.unwrap_or(false) { 1 } else { 0 },
        if req.tools.unwrap_or(true) { 1 } else { 0 },
    ).await {
        return Json(serde_json::json!({"ok": false, "error": e.to_string()}));
    }
    let mut pm = state.provider_manager.write().await;
    let _ = pm.reload_custom_provider(&id, &state.db).await;
    Json(serde_json::json!({"ok": true}))
}

pub async fn api_remove_custom_model(
    State(state): State<Arc<AppState>>,
    Path((id, model_id)): Path<(String, String)>,
) -> Json<serde_json::Value> {
    let _ = crate::db::remove_custom_provider_model(&state.db, &id, &model_id).await;
    let mut pm = state.provider_manager.write().await;
    let _ = pm.reload_custom_provider(&id, &state.db).await;
    Json(serde_json::json!({"ok": true}))
}

pub async fn api_delete_custom_provider(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let deleted = crate::db::delete_custom_provider(&state.db, &id).await.unwrap_or(false);
    if deleted {
        let mut pm = state.provider_manager.write().await;
        let _ = pm.reload_custom_provider(&id, &state.db).await;
    }
    Json(serde_json::json!({"ok": deleted}))
}

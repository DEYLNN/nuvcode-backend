use std::sync::Arc;

use axum::{
    extract::{Path, State},
    Json,
};

use crate::state::AppState;

/// GET /admin/api/providers/:id
///
/// Returns provider metadata + per-provider model list (with blocked flags) + key list
/// (with runtime lock state from the manager).
pub async fn api_provider_detail(
    State(state): State<Arc<AppState>>,
    Path(provider_id): Path<String>,
) -> Json<serde_json::Value> {
    let pm = state.provider_manager.read().await;
    if let Some(p) = pm.get(&provider_id) {
        let meta = p.metadata();
        let total_keys =
            crate::db::count_provider_keys(&state.db, &provider_id, false).await;
        let active_keys =
            crate::db::count_provider_keys(&state.db, &provider_id, true).await;
        let locked_keys = total_keys - active_keys;
        let key_type: String = meta.category.clone();

        let mut models: Vec<serde_json::Value> = match p.list_models().await {
            Ok(list) => {
                let blocked: std::collections::HashSet<String> = sqlx::query_scalar(
                    "SELECT model_id FROM blocked_models WHERE provider_id = $1",
                )
                .bind(&provider_id)
                .fetch_all(&state.db)
                .await
                .unwrap_or_default()
                .into_iter()
                .collect();

                list.into_iter()
                    .map(|m| {
                        let model_name = m.id.strip_prefix(&format!("{}/", provider_id)).unwrap_or(&m.id).to_string();
                        serde_json::json!({
                            "id": m.id,
                            "name": model_name,
                            "available": true,
                            "blocked": blocked.contains(&model_name),
                            "context_length": m.context_length,
                        })
                    })
                    .collect::<Vec<_>>()
            }
            _ => vec![],
        };
        // Merge user-added custom models (same pattern as manager.list_all_models)
        // so they appear in the UI model list with a delete button.
        // Dedup guard: engine list_models() may already emit the model under the
        // UI prefix (e.g. `nx/mimo-v2.5`); a custom provider's DB id is
        // `custom_nx`, so matching on the raw provider_id misses it and the
        // model shows twice (`nx/x` + `custom_nx/x`). Compare by model NAME
        // (suffix after `/`) too.
        let custom_prefix = meta
            .model_prefix
            .clone()
            .unwrap_or_else(|| provider_id.clone());
        for cm in crate::db::list_custom_models(&state.db, &provider_id).await {
            let id = format!("{}/{}", custom_prefix, cm.model_id);
            let already = models.iter().any(|m| {
                m["id"] == serde_json::Value::String(id.clone())
                    || m["name"] == serde_json::Value::String(cm.model_id.clone())
            });
            if already {
                continue;
            }
            models.push(serde_json::json!({
                "id": id,
                "name": cm.model_id,
                "available": true,
                "blocked": false,
                "context_length": cm.ctx as u32,
            }));
        }

        let keys: Vec<serde_json::Value> = sqlx::query_as::<_, (String, String, Option<String>, String, bool)>(
            "SELECT id, key_value, label, key_type, is_active FROM api_keys WHERE provider_id = $1 ORDER BY created_at DESC",
        )
        .bind(&provider_id)
        .fetch_all(&state.db)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|(id, key_value, label, key_type, is_active)| {
            let preview = if key_value.len() > 12 {
                format!("{}...{}", &key_value[..6], &key_value[key_value.len() - 4..])
            } else {
                key_value
            };
            serde_json::json!({
                "id": id,
                "label": label,
                "key_type": key_type,
                "is_active": is_active,
                "is_locked": !is_active,
                "masked": preview,
            })
        })
        .collect();

        let runtime_locked: std::collections::HashMap<String, (u64, String)> = p
            .locked_keys()
            .into_iter()
            .map(|(k, s, r)| (k, (s, r)))
            .collect();
        let keys: Vec<serde_json::Value> = keys
            .into_iter()
            .map(|mut k| {
                if let Some(id) = k["id"].as_str() {
                    if let Some((remaining, reason)) = runtime_locked.get(id) {
                        k["is_locked"] = serde_json::Value::Bool(true);
                        k["locked_reason"] = serde_json::Value::String(reason.clone());
                        k["locked_remaining"] =
                            serde_json::Value::Number(serde_json::Number::from(*remaining));
                    }
                }
                k
            })
            .collect();

        Json(serde_json::json!({
            "id": provider_id,
            "display_name": meta.display_name,
            "color": meta.color,
            "icon_name": meta.icon_name,
            "category": meta.category,
            "base_url": meta.version,
            "validate_url": meta.validate_url,
            "total_keys": total_keys,
            "active_keys": active_keys,
            "locked_keys": locked_keys,
            "type": key_type,
            "oauth_flow": meta.oauth_flow,
            "description": "",
            "models": models,
            "keys": keys,
        }))
    } else {
        Json(serde_json::json!({"error": "Provider not found"}))
    }
}

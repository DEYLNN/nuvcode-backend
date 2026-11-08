use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    Json,
};

use crate::state::AppState;

/// GET /admin/api/providers/:id/validate-models?key_id=xxx
///
/// Proxies to the provider's validate_url using the first active API key (or the one
/// specified via key_id). Returns a normalized model list extracted from various
/// OpenAI-style response shapes.
pub async fn api_validate_models(
    State(state): State<Arc<AppState>>,
    Path(provider_id): Path<String>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let key_id = params.get("key_id").map(|s| s.as_str());

    let key_row = if let Some(kid) = key_id {
        sqlx::query_as::<_, (String,)>(
            "SELECT key_value FROM api_keys WHERE id = $1 AND provider_id = $2 AND is_active = true LIMIT 1",
        )
        .bind(kid)
        .bind(&provider_id)
        .fetch_optional(&state.db)
        .await
        .unwrap_or(None)
    } else {
        sqlx::query_as::<_, (String,)>(
            "SELECT key_value FROM api_keys WHERE provider_id = $1 AND is_active = true LIMIT 1",
        )
        .bind(&provider_id)
        .fetch_optional(&state.db)
        .await
        .unwrap_or(None)
    };

    let key_value = match key_row {
        Some((kv,)) => kv,
        None => {
            return Json(
                serde_json::json!({"ok": false, "error": "No active API key"}),
            );
        }
    };

    let pm = state.provider_manager.read().await;
    let meta = pm.get(&provider_id).map(|p| p.metadata());
    let validate_url = meta
        .as_ref()
        .map(|m| m.validate_url.clone())
        .unwrap_or_default();
    drop(pm);

    if validate_url.is_empty() {
        return Json(
            serde_json::json!({"ok": false, "error": "No validate_url for this provider"}),
        );
    }

    let client = reqwest::Client::new();
    let resp = match client
        .get(&validate_url)
        .header("Authorization", format!("Bearer {}", key_value))
        .header("User-Agent", "NuvCode/1.0")
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return Json(
                serde_json::json!({"ok": false, "error": format!("HTTP error: {}", e)}),
            );
        }
    };

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Json(
            serde_json::json!({"ok": false, "error": format!("HTTP {}: {}", status, body)}),
        );
    }

    let body = match resp.text().await {
        Ok(b) => b,
        Err(e) => {
            return Json(
                serde_json::json!({"ok": false, "error": format!("Read body: {}", e)}),
            );
        }
    };

    let parsed = serde_json::from_str::<serde_json::Value>(&body);
    match parsed {
        Ok(json) => {
            let models = try_extract_models(&json);
            Json(serde_json::json!({
                "ok": true,
                "models": models,
                "raw": json,
            }))
        }
        Err(_) => Json(serde_json::json!({
            "ok": true,
            "models": [],
            "raw": body,
        })),
    }
}

/// Try to extract model list from various JSON response shapes.
/// Supports: `{"data":[...]}`, `{"models":[...]}`, plain array.
fn try_extract_models(v: &serde_json::Value) -> Vec<serde_json::Value> {
    if let Some(arr) = v.get("data").and_then(|d| d.as_array()) {
        let has_id = arr.iter().any(|m| m.get("id").is_some());
        if has_id {
            return arr.iter().map(normalize_model).collect();
        }
    }
    if let Some(arr) = v.get("models").and_then(|d| d.as_array()) {
        return arr.iter().map(normalize_model).collect();
    }
    if let Some(arr) = v.as_array() {
        if arr.iter().any(|m| m.get("id").is_some()) {
            return arr.iter().map(normalize_model).collect();
        }
    }
    vec![]
}

fn normalize_model(m: &serde_json::Value) -> serde_json::Value {
    let id = m.get("id").and_then(|s| s.as_str()).unwrap_or("?");
    serde_json::json!({
        "id": id,
        "name": m.get("name").and_then(|s| s.as_str()).unwrap_or(id),
        "owned_by": m.get("owned_by").and_then(|s| s.as_str()),
        "context_length": m.get("context_length").or_else(|| m.get("max_tokens")),
    })
}

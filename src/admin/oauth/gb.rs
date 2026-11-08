use std::sync::Arc;
use axum::extract::{Path, State};
use axum::Json;
use crate::state::AppState;

pub async fn start() -> Json<serde_json::Value> {
    match crate::providers::oauth::grok_build::oauth::start().await {
        Ok(data) => Json(data),
        Err(e) => Json(serde_json::json!({"error": e})),
    }
}

pub async fn poll(State(state): State<Arc<AppState>>, Json(body): Json<serde_json::Value>) -> Json<serde_json::Value> {
    let device_code = body.get("device_code").and_then(|v| v.as_str()).unwrap_or_default();
    match crate::providers::oauth::grok_build::oauth::poll(device_code).await {
        Ok(data) => {
            if data.get("access_token").is_some() {
                if let Err(e) = crate::providers::oauth::grok_build::oauth::save_token(&state, &data).await { return Json(serde_json::json!({"error": e})); }
                return Json(serde_json::json!({"ok": true, "token": data["access_token"]}));
            }
            Json(data)
        }
        Err(e) => Json(serde_json::json!({"error": e})),
    }
}

pub async fn refresh(State(state): State<Arc<AppState>>, Path(key_id): Path<String>) -> Json<serde_json::Value> {
    let row = sqlx::query_as::<_, (String,)>("SELECT key_value FROM api_keys WHERE id = $1 AND provider_id = 'gb' AND key_type = 'oauth' AND is_active = true")
        .bind(&key_id).fetch_optional(&state.db).await.unwrap_or(None);
    let Some((raw,)) = row else { return Json(serde_json::json!({"ok": false, "error": "Grok Build key not found"})); };
    let old: serde_json::Value = match serde_json::from_str(&raw) { Ok(v) => v, Err(_) => return Json(serde_json::json!({"ok": false, "error": "Invalid credential JSON"})) };
    let Some(rt) = old.get("refresh_token").and_then(|v| v.as_str()) else { return Json(serde_json::json!({"ok": false, "error": "Refresh token missing"})); };
    match crate::providers::oauth::grok_build::oauth::refresh(rt).await {
        Ok(data) => {
            let Some(access) = data.get("access_token").and_then(|v| v.as_str()) else { return Json(serde_json::json!({"ok": false, "error": "Refresh response missing access_token"})); };
            let exp = data.get("expires_in").and_then(|v| v.as_i64()).map(|s| chrono::Utc::now().timestamp() + s).or_else(|| old.get("expires_at").and_then(|v| v.as_i64()));
            let value = serde_json::json!({"access_token": access, "refresh_token": data.get("refresh_token").and_then(|v| v.as_str()).unwrap_or(rt), "expires_at": exp, "email": old.get("email")});
            if let Err(e) = sqlx::query("UPDATE api_keys SET key_value = $1, updated_at = $2 WHERE id = $3").bind(value.to_string()).bind(chrono::Utc::now().to_rfc3339()).bind(&key_id).execute(&state.db).await { return Json(serde_json::json!({"ok": false, "error": format!("DB update failed: {e}")})); }
            Json(serde_json::json!({"ok": true, "expires_at": exp}))
        }
        Err(e) => Json(serde_json::json!({"ok": false, "error": e})),
    }
}

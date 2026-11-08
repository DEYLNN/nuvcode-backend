use std::sync::Arc;
use serde_json::Value;
use crate::state::AppState;

const POLL_URL_BASE: &str = "https://api.kilo.ai/api/device-auth/codes";
const PROFILE_URL: &str = "https://api.kilo.ai/api/profile";

pub async fn start() -> Result<Value, String> {
    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36")
        .build()
        .map_err(|e| format!("KiloCode client: {e}"))?;
    let resp = client
        .post("https://api.kilo.ai/api/device-auth/codes")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("Origin", "https://app.kilo.ai")
        .header("Referer", "https://app.kilo.ai/")
        .body("{}")
        .send()
        .await
        .map_err(|e| format!("KiloCode initiate: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("KiloCode API HTTP {status}: {body}"));
    }
    let text = resp.text().await.map_err(|e| format!("KiloCode body: {e}"))?;
    if text.is_empty() {
        return Err("KiloCode API returned empty body".into());
    }
    let data: Value = serde_json::from_str(&text).map_err(|e| format!("KiloCode parse: {e}"))?;

    Ok(serde_json::json!({
        "device_code": data["code"],
        "user_code": data["user_code"],
        "verification_uri": data["verificationUrl"],
        "verification_uri_complete": data["verificationUrl"],
        "expires_in": data["expiresIn"].as_i64().unwrap_or(599),
        "interval": 3,
    }))
}

pub async fn poll(device_code: &str) -> Result<Value, String> {
    let url = format!("{POLL_URL_BASE}/{device_code}");
    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("KiloCode poll HTTP: {e}"))?;

    // Map HTTP status
    if resp.status() == 202 {
        return Ok(serde_json::json!({"error": "authorization_pending"}));
    }
    if resp.status() == 403 {
        return Ok(serde_json::json!({"error": "access_denied", "error_description": "Authorization denied by user"}));
    }
    if resp.status() == 410 {
        return Ok(serde_json::json!({"error": "expired_token", "error_description": "Authorization code expired"}));
    }
    if !resp.status().is_success() {
        return Ok(serde_json::json!({"error": "poll_failed", "error_description": format!("Poll failed: {}", resp.status())}));
    }

    let text = resp.text().await.map_err(|e| format!("KiloCode body: {e}"))?;
    let data: Value = serde_json::from_str(&text).map_err(|e| format!("KiloCode parse: {e}"))?;

    if data["status"] == "approved" {
        let token = data["token"].as_str().unwrap_or_default();
        // Fetch profile for org ID
        let mut org_id = None;
        if !token.is_empty() {
            let profile_resp = client
                .get(PROFILE_URL)
                .header("Authorization", format!("Bearer {token}"))
                .send()
                .await;
            if let Ok(pr) = profile_resp {
                if pr.status().is_success() {
                    if let Ok(profile) = pr.json::<Value>().await {
                        org_id = profile["organizations"]
                            .as_array()
                            .and_then(|arr| arr.first())
                            .and_then(|o| o["id"].as_str())
                            .map(String::from);
                    }
                }
            }
        }
        return Ok(serde_json::json!({
            "ok": true,
            "access_token": token,
            "_userEmail": data["userEmail"],
            "_orgId": org_id,
        }));
    }

    Ok(serde_json::json!({"error": "authorization_pending"}))
}

pub async fn save_token(state: &Arc<AppState>, data: &Value) -> Result<(), String> {
    let kid = format!("key_{}", uuid::Uuid::new_v4().to_string().split('-').next().unwrap());
    let now = chrono::Utc::now().to_rfc3339();
    let access_token = data["access_token"].as_str().unwrap_or_default();
    let email = data["_userEmail"].as_str().or_else(|| data["email"].as_str()).unwrap_or("kilocode");
    let org_id = data["_orgId"].as_str().map(String::from);

    let mut kv = serde_json::json!({
        "access_token": access_token,
        "email": email,
    });
    if let Some(oid) = org_id {
        kv["orgId"] = serde_json::Value::String(oid);
    }
    let kv_str = serde_json::to_string(&kv).map_err(|e| format!("KiloCode serialize: {e}"))?;

    sqlx::query(
        "INSERT INTO api_keys (id, provider_id, key_value, label, is_active, key_type, created_at, updated_at) VALUES ($1, 'kc', $2, $3, true, 'oauth', $4, $5)",
    )
    .bind(&kid)
    .bind(&kv_str)
    .bind(email)
    .bind(&now)
    .bind(&now)
    .execute(&state.db)
    .await
    .map_err(|e| format!("KiloCode DB: {e}"))?;

    let _ = state.provider_manager.write().await.reload_provider("kc").await;
    Ok(())
}

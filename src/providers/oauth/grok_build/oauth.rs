use serde_json::{json, Value};
use std::sync::Arc;

use super::constants;
use crate::state::AppState;

/// Kick off xAI OAuth device_code flow.
///
/// Returns the standard OAuth device_code payload that FE expects.
/// FE polls `/oauth/gb/poll` until ready.
pub async fn start() -> Result<Value, String> {
    let client = reqwest::Client::builder()
        .user_agent(constants::GROK_CLI_USER_AGENT)
        .build()
        .map_err(|e| format!("GrokBuild client: {e}"))?;

    let resp = client
        .post(constants::OAUTH_DEVICE_CODE_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .form(&[
            ("client_id", constants::OAUTH_CLIENT_ID),
            ("scope", constants::OAUTH_SCOPES),
            ("response_type", "code"),
        ])
        .send()
        .await
        .map_err(|e| format!("GrokBuild device-code initiate: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("GrokBuild device-code HTTP {status}: {body}"));
    }

    let data: Value = resp.json().await.map_err(|e| format!("GrokBuild parse: {e}"))?;

    Ok(json!({
        "device_code": data["device_code"],
        "user_code": data["user_code"],
        "verification_uri": data["verification_uri"],
        "verification_uri_complete": data["verification_uri_complete"],
        "expires_in": data["expires_in"].as_i64().unwrap_or(300),
        "interval": constants::OAUTH_POLL_INTERVAL_SECS,
    }))
}

/// Poll xAI token endpoint with the device_code. Returns either a pending
/// status or the full token bundle on success.
pub async fn poll(device_code: &str) -> Result<Value, String> {
    let client = reqwest::Client::builder()
        .user_agent(constants::GROK_CLI_USER_AGENT)
        .build()
        .map_err(|e| format!("GrokBuild poll client: {e}"))?;

    let resp = client
        .post(constants::OAUTH_TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .form(&[
            ("client_id", constants::OAUTH_CLIENT_ID),
            ("device_code", device_code),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ])
        .send()
        .await
        .map_err(|e| format!("GrokBuild poll: {e}"))?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();

    if !status.is_success() {
        return Err(format!("GrokBuild poll HTTP {status}: {text}"));
    }

    let data: Value = serde_json::from_str(&text)
        .map_err(|e| format!("GrokBuild poll parse: {e} — body: {}", &text[..text.len().min(200)]))?;

    Ok(data)
}

/// Refresh an access token using a refresh_token.
///
/// Returns the refreshed token bundle in the same shape as `poll`.
pub async fn refresh(refresh_token: &str) -> Result<Value, String> {
    let client = reqwest::Client::builder()
        .user_agent(constants::GROK_CLI_USER_AGENT)
        .build()
        .map_err(|e| format!("GrokBuild refresh client: {e}"))?;

    let resp = client
        .post(constants::OAUTH_TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .form(&[
            ("client_id", constants::OAUTH_CLIENT_ID),
            ("refresh_token", refresh_token),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .await
        .map_err(|e| format!("GrokBuild refresh: {e}"))?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("GrokBuild refresh HTTP {status}: {text}"));
    }

    let data: Value = serde_json::from_str(&text)
        .map_err(|e| format!("GrokBuild refresh parse: {e} — body: {}", &text[..text.len().min(200)]))?;

    Ok(data)
}

/// Persist token bundle to api_keys as JSON (same pattern as KC).
/// Stashes access_token, refresh_token, expires_at, email.
pub async fn save_token(state: &Arc<AppState>, data: &Value) -> Result<(), String> {
    let kid = format!("key_{}", uuid::Uuid::new_v4().to_string().split('-').next().unwrap());
    let now = chrono::Utc::now().to_rfc3339();
    let access_token = data["access_token"].as_str().unwrap_or_default();
    let refresh_token = data["refresh_token"].as_str();
    let expires_at = data["expires_in"].as_i64().map(|secs| {
        chrono::Utc::now().timestamp() + secs
    });
    let email = data["email"].as_str();
    let short = uuid::Uuid::new_v4().to_string().split('-').next().unwrap().to_string();
    let label = email
        .map(|e| format!("gb-{}", e))
        .unwrap_or_else(|| format!("gb-{}", &short[..6]));

    let mut kv = serde_json::json!({
        "access_token": access_token,
    });
    if let Some(e) = email {
        kv["email"] = serde_json::Value::String(e.to_string());
    }
    if let Some(rt) = refresh_token {
        kv["refresh_token"] = serde_json::Value::String(rt.to_string());
    }
    if let Some(exp) = expires_at {
        kv["expires_at"] = serde_json::Value::Number(exp.into());
    }
    let kv_str = serde_json::to_string(&kv).map_err(|e| format!("GrokBuild serialize: {e}"))?;

    sqlx::query(
        "INSERT INTO api_keys (id, provider_id, key_value, label, is_active, key_type, created_at, updated_at) VALUES ($1, 'gb', $2, $3, true, 'oauth', $4, $5)",
    )
    .bind(&kid)
    .bind(&kv_str)
    .bind(&label)
    .bind(&now)
    .bind(&now)
    .execute(&state.db)
    .await
    .map_err(|e| format!("GrokBuild DB: {e}"))?;

    let _ = state.provider_manager.write().await.reload_provider("gb").await;
    Ok(())
}

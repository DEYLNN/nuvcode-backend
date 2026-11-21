use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::state::AppState;

use super::constants;

/// In-memory mapping: oauth_state → () (just track validity).
/// Single-instance process (dev); a multi-instance deploy would push this
/// to Redis — same ceiling note as codex.
fn state_store() -> &'static Mutex<HashMap<String, ()>> {
    static STORE: OnceLock<Mutex<HashMap<String, ()>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Serialize)]
pub struct OAuthStartResponse {
    pub url: String,
    pub state: String,
}

#[derive(Deserialize)]
#[allow(non_snake_case)]
pub struct CallbackRequest {
    pub api_key: Option<String>,
    pub apiKey: Option<String>,
    pub user_id: Option<String>,
    pub userId: Option<String>,
    pub user_name: Option<String>,
    pub userName: Option<String>,
    pub state: Option<String>,
}

#[derive(Deserialize)]
#[allow(non_snake_case)]
pub struct ManualTokenRequest {
    pub api_key: Option<String>,
    pub apiKey: Option<String>,
}

/// Kick off the browser hand-off flow.
/// Returns the studio auth URL with the callback pointing to localhost
/// (Command Code studio only accepts localhost URLs for security).
pub async fn start(_public_url: &str) -> OAuthStartResponse {
    let oauth_state = uuid::Uuid::new_v4().to_string();
    state_store()
        .lock()
        .unwrap()
        .insert(oauth_state.clone(), ());

    // Command Code studio only accepts localhost callback URLs.
    // The callback hits our backend directly (browser → localhost:7444).
    let callback = "http://localhost:7444/admin/api/oauth/cc/callback".to_string();
    let url = format!(
        "{}?callback={}&state={}&mode=redirect",
        constants::STUDIO_AUTH_URL,
        urlencoding::encode(&callback),
        oauth_state,
    );

    OAuthStartResponse {
        url,
        state: oauth_state,
    }
}

/// Handle the POST callback from the studio page.
/// Validates the state, extracts apiKey, saves to DB.
pub async fn handle_callback(
    state: &Arc<AppState>,
    req: &CallbackRequest,
) -> Result<(), String> {
    let oauth_state = req.state.as_deref().unwrap_or("");
    if oauth_state.is_empty() {
        return Err("CommandCode: missing state".into());
    }

    // Validate state
    {
        let mut store = state_store().lock().unwrap();
        if !store.remove(oauth_state).is_some() {
            return Err("CommandCode: invalid or expired state".into());
        }
    }

    let api_key = req
        .api_key
        .as_deref()
        .or(req.apiKey.as_deref())
        .ok_or_else(|| "CommandCode: missing apiKey".to_string())?;
    if api_key.is_empty() {
        return Err("CommandCode: empty apiKey".into());
    }

    let user_id = req.user_id.as_deref().or(req.userId.as_deref());
    let user_name = req.user_name.as_deref().or(req.userName.as_deref());

    // Validate the token with /alpha/whoami
    validate_token(api_key).await?;

    save_token(state, api_key, user_id, user_name).await
}

/// Manual token paste — user provides apiKey directly.
/// Validate with /alpha/whoami, save to DB.
pub async fn handle_manual(
    state: &Arc<AppState>,
    req: &ManualTokenRequest,
) -> Result<(), String> {
    let api_key = req
        .api_key
        .as_deref()
        .or(req.apiKey.as_deref())
        .ok_or_else(|| "CommandCode: missing apiKey".to_string())?;
    if api_key.is_empty() {
        return Err("CommandCode: empty apiKey".into());
    }

    // Validate the token
    let (user_id, user_name) = validate_token_detail(api_key).await?;

    save_token(state, api_key, user_id.as_deref(), user_name.as_deref()).await
}

/// Validate token with GET /alpha/whoami. Returns Ok if valid.
pub async fn validate_token(api_key: &str) -> Result<(), String> {
    let client = reqwest::Client::new();
    let resp = client
        .get(constants::WHOAMI_URL)
        .header("Authorization", format!("Bearer {api_key}"))
        .header("User-Agent", constants::USER_AGENT)
        .header("x-command-code-version", constants::CLI_VERSION)
        .header("x-cli-environment", constants::CLI_ENVIRONMENT)
        .send()
        .await
        .map_err(|e| format!("CommandCode whoami HTTP: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("CommandCode whoami HTTP {status}: {body}"));
    }

    Ok(())
}

/// Validate token and return user details from /alpha/whoami.
async fn validate_token_detail(api_key: &str) -> Result<(Option<String>, Option<String>), String> {
    let client = reqwest::Client::new();
    let resp = client
        .get(constants::WHOAMI_URL)
        .header("Authorization", format!("Bearer {api_key}"))
        .header("User-Agent", constants::USER_AGENT)
        .header("x-command-code-version", constants::CLI_VERSION)
        .header("x-cli-environment", constants::CLI_ENVIRONMENT)
        .send()
        .await
        .map_err(|e| format!("CommandCode whoami HTTP: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("CommandCode whoami HTTP {status}: {body}"));
    }

    let data: serde_json::Value =
        serde_json::from_str(&resp.text().await.unwrap_or_default()).unwrap_or(json!({}));
    let user_id = data
        .get("userId")
        .or_else(|| data.get("user_id"))
        .and_then(|v| v.as_str())
        .map(String::from);
    let user_name = data
        .get("userName")
        .or_else(|| data.get("user_name"))
        .or_else(|| data.get("name"))
        .and_then(|v| v.as_str())
        .map(String::from);

    Ok((user_id, user_name))
}

/// Persist the API key as an api_keys row (oauth type).
async fn save_token(
    state: &Arc<AppState>,
    api_key: &str,
    user_id: Option<&str>,
    user_name: Option<&str>,
) -> Result<(), String> {
    let kid = format!(
        "key_{}",
        uuid::Uuid::new_v4().to_string().split('-').next().unwrap()
    );
    let now = chrono::Utc::now().to_rfc3339();

    let mut kv = json!({
        "access_token": api_key,
    });
    if let Some(uid) = user_id {
        kv["userId"] = json!(uid);
    }
    if let Some(uname) = user_name {
        kv["userName"] = json!(uname);
    }
    let kv_str =
        serde_json::to_string(&kv).map_err(|e| format!("CommandCode serialize: {e}"))?;

    let label = user_name
        .or(user_id)
        .unwrap_or("commandcode")
        .to_string();

    sqlx::query(
        "INSERT INTO api_keys (id, provider_id, key_value, label, is_active, key_type, created_at, updated_at) VALUES ($1, 'cc', $2, $3, true, 'oauth', $4, $5)",
    )
    .bind(&kid)
    .bind(&kv_str)
    .bind(&label)
    .bind(&now)
    .bind(&now)
    .execute(&state.db)
    .await
    .map_err(|e| format!("CommandCode DB: {e}"))?;

    let _ = state
        .provider_manager
        .write()
        .await
        .reload_provider("cc")
        .await;
    Ok(())
}

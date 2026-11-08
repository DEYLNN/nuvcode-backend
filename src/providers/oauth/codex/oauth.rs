use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

use crate::state::AppState;

/// In-memory mapping oauth_state → code_verifier for PKCE S256.
/// Single-instance process (dev); a multi-instance deploy would push this
/// to Redis — see ponytail.rs ceiling note.
fn pkce_store() -> &'static Mutex<HashMap<String, String>> {
    static STORE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Serialize)]
pub struct OAuthStartResponse {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

/// Kick off authorization_code + PKCE flow.
/// Returns the browser authorize URL + the state (needed for manual code exchange).
pub async fn start() -> OAuthStartResponse {
    use rand::RngCore;
    let oauth_state = uuid::Uuid::new_v4().to_string();
    let mut code_verifier_bytes = [0u8; 64];
    rand::thread_rng().fill_bytes(&mut code_verifier_bytes);
    let code_verifier = URL_SAFE_NO_PAD.encode(&code_verifier_bytes);
    let code_challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(code_verifier.as_bytes()).as_slice());

    pkce_store().lock().unwrap().insert(oauth_state.clone(), code_verifier);

    let client_id = super::constants::CLIENT_ID;
    let redirect_uri = urlencoding::encode(super::constants::REDIRECT_URL);
    let scope = urlencoding::encode(super::constants::OAUTH_SCOPE);

    let url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&code_challenge={}&code_challenge_method=S256&{}",
        super::constants::OAUTH_AUTHORIZE_URL,
        client_id,
        redirect_uri,
        scope,
        oauth_state,
        code_challenge,
        super::constants::OAUTH_EXTRA_PARAMS,
    );

    OAuthStartResponse { url, id: Some(oauth_state) }
}

#[derive(Deserialize)]
pub struct ManualCodeRequest {
    pub code: String,
    #[serde(alias = "id")]
    pub state: Option<String>,
}

/// Exchange an authorization code for a token bundle (POST /oauth/token).
///
/// If `oauth_state` is empty/missing (FE manual paste drops the state param),
/// fall back to the single outstanding PKCE entry — safe for single-flow UX.
pub async fn exchange_code(
    code: &str,
    oauth_state: &str,
) -> Result<serde_json::Value, String> {
    let code_verifier = {
        let mut store = pkce_store().lock().unwrap();
        if !oauth_state.is_empty() {
            store.remove(oauth_state)
        } else if store.len() == 1 {
            // FE manual paste sends only {code}; recover the sole verifier.
            let (_, verifier) = store.drain().next().unwrap();
            Some(verifier)
        } else {
            None
        }
    }
    .unwrap_or_default();
    let client_id = super::constants::CLIENT_ID;
    let redirect_uri = super::constants::REDIRECT_URL;

    let client = reqwest::Client::new();
    let resp = client
        .post(super::constants::OAUTH_TOKEN_URL)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", client_id),
            ("code_verifier", &code_verifier),
        ])
        .send()
        .await
        .map_err(|e| format!("Codex exchange HTTP: {e}"))?;

    let status = resp.status();
    let text = resp.text().await.map_err(|e| format!("Codex exchange body: {e}"))?;
    if !status.is_success() {
        return Err(format!("Codex exchange HTTP {status}: {text}"));
    }

    let token: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("Codex exchange parse: {e} — body: {text}"))?;

    if token.get("error").is_some() {
        return Err(format!("{:?}", token.get("error")));
    }

    Ok(token)
}

/// Refresh an access token using a refresh_token.
/// Returns the refreshed token bundle in the same shape as exchange_code.
pub async fn exchange_refresh(refresh_token: &str) -> Result<serde_json::Value, String> {
    let client = reqwest::Client::new();
    let resp = client
        .post(super::constants::OAUTH_TOKEN_URL)
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", super::constants::CLIENT_ID),
            ("refresh_token", refresh_token),
        ])
        .send()
        .await
        .map_err(|e| format!("Codex refresh HTTP: {e}"))?;

    let status = resp.status();
    let text = resp.text().await.map_err(|e| format!("Codex refresh body: {e}"))?;
    if !status.is_success() {
        return Err(format!("Codex refresh HTTP {status}: {text}"));
    }

    let token: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("Codex refresh parse: {e} — body: {text}"))?;

    if token.get("error").is_some() {
        return Err(format!("{:?}", token.get("error")));
    }

    Ok(token)
}

/// Enrich token with absolute expires_at then persist as api_keys JSON.
pub async fn save_token(state: &Arc<AppState>, token: &serde_json::Value) -> Result<(), String> {
    let kid = format!("key_{}", uuid::Uuid::new_v4().to_string().split('-').next().unwrap());
    let now = chrono::Utc::now().to_rfc3339();

    let mut enriched = token.clone();
    if let Some(exp_in) = token.get("expires_in").and_then(|v| v.as_u64()) {
        let exp_at = chrono::Utc::now() + chrono::Duration::seconds(exp_in as i64);
        enriched.as_object_mut().map(|obj| {
            obj.insert("expires_at".into(), serde_json::Value::String(exp_at.to_rfc3339()));
        });
    }
    let kv = serde_json::to_string(&enriched).map_err(|e| format!("Codex serialize: {e}"))?;

    // Extract email from id_token for the label (JWT payload).
    let label = extract_email_from_id_token(token)
        .unwrap_or_else(|| format!("codex-{}", &kid[4..12]));

    sqlx::query("INSERT INTO api_keys (id, provider_id, key_value, label, is_active, key_type, created_at, updated_at) VALUES ($1, 'cx', $2, $3, true, 'oauth', $4, $5)")
        .bind(&kid).bind(&kv).bind(&label).bind(&now).bind(&now)
        .execute(&state.db).await.map_err(|e| format!("Codex DB: {e}"))?;

    let _ = state.provider_manager.write().await.reload_provider("cx").await;
    Ok(())
}

fn extract_email_from_id_token(token: &serde_json::Value) -> Option<String> {
    let s = token.get("id_token").and_then(|t| t.as_str())?;
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let decoded = URL_SAFE_NO_PAD.decode(parts[1]).ok()?;
    let payload: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    payload.get("email").and_then(|v| v.as_str()).map(|e| e.to_string())
}
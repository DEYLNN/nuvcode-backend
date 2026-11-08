use serde_json::{json, Value};
use std::sync::Arc;
use crate::state::AppState;
use super::constants;

fn headers(b: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    b.header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("User-Agent", constants::USER_AGENT)
        .header("X-Requested-With", "XMLHttpRequest")
        .header("X-Domain", "www.codebuddy.ai")
        .header("X-No-Authorization", "true")
        .header("X-No-User-Id", "true")
        .header("X-Product", "SaaS")
}

pub async fn start() -> Result<Value, String> {
    let c=reqwest::Client::new();
    let r=headers(c.post(constants::STATE_URL)).body("{}").send().await.map_err(|e|format!("CodeBuddy start: {e}"))?;
    let status=r.status(); let text=r.text().await.unwrap_or_default();
    if !status.is_success(){return Err(format!("CodeBuddy start HTTP {status}: {text}"));}
    let d:Value=serde_json::from_str(&text).map_err(|e|format!("CodeBuddy start parse: {e}"))?;
    if d["code"] != 0 || d["data"]["state"].as_str().is_none() || d["data"]["authUrl"].as_str().is_none(){return Err(format!("CodeBuddy state error: {}",d["msg"].as_str().unwrap_or("missing state/authUrl")));}
    Ok(json!({"device_code":d["data"]["state"],"user_code":"","verification_uri":d["data"]["authUrl"],"verification_uri_complete":d["data"]["authUrl"],"expires_in":900,"interval":5}))
}

pub async fn poll(state:&str)->Result<Value,String>{
    let c=reqwest::Client::new();
    let r=headers(c.get(format!("{}?state={}",constants::TOKEN_URL,urlencoding::encode(state)))).header("X-No-Enterprise-Id","true").header("X-No-Department-Info","true").send().await.map_err(|e|format!("CodeBuddy poll: {e}"))?;
    let text=r.text().await.unwrap_or_default(); let d:Value=serde_json::from_str(&text).map_err(|e|format!("CodeBuddy poll parse: {e}"))?;
    if d["code"]==0 && d["data"]["accessToken"].as_str().is_some(){return Ok(json!({"ok":true,"access_token":d["data"]["accessToken"],"refresh_token":d["data"]["refreshToken"],"expires_in":d["data"]["expiresIn"],"email":d["data"]["email"]}));}
    if d["code"]==11217{return Ok(json!({"error":"authorization_pending"}));}
    Ok(json!({"error":d["msg"].as_str().unwrap_or("authorization_failed")}))
}

pub async fn refresh(refresh_token:&str)->Result<Value,String>{
    let c=reqwest::Client::new();
    let r=headers(c.post(constants::REFRESH_URL)).header("X-Refresh-Token",refresh_token).header("X-Auth-Refresh-Source","plugin").body("{}").send().await.map_err(|e|format!("CodeBuddy refresh: {e}"))?;
    let status=r.status();let text=r.text().await.unwrap_or_default();if !status.is_success(){return Err(format!("CodeBuddy refresh HTTP {status}: {text}"));}
    let d:Value=serde_json::from_str(&text).map_err(|e|format!("CodeBuddy refresh parse: {e}"))?;if d["code"]!=0||d["data"]["accessToken"].as_str().is_none(){return Err(d["msg"].as_str().unwrap_or("refresh failed").into());}
    Ok(json!({"access_token":d["data"]["accessToken"],"refresh_token":d["data"]["refreshToken"],"expires_in":d["data"]["expiresIn"]}))
}

pub async fn save_token(state:&Arc<AppState>,data:&Value)->Result<(),String>{
    let id=format!("key_{}",&uuid::Uuid::new_v4().to_string()[..8]);let now=chrono::Utc::now().to_rfc3339();
    let value=json!({"access_token":data["access_token"],"refresh_token":data["refresh_token"],"expires_at":data["expires_in"].as_i64().map(|s|chrono::Utc::now().timestamp()+s),"email":data["email"]}).to_string();
    sqlx::query("INSERT INTO api_keys (id,provider_id,key_value,label,is_active,key_type,created_at,updated_at) VALUES ($1, 'cbai', $2, 'CodeBuddy', true, 'oauth', $3, $4)").bind(&id).bind(value).bind(&now).bind(&now).execute(&state.db).await.map_err(|e|format!("CodeBuddy DB: {e}"));
    let _=state.provider_manager.write().await.reload_provider("cbai").await;Ok(())
}

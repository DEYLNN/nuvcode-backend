/// Credentials for Command Code.
///
/// Command Code uses a static API key (Bearer token) obtained via the
/// browser hand-off OAuth flow. The studio page POSTs back
/// `{ apiKey, userId, userName, state }`; we persist `apiKey` as the
/// access token plus optional metadata (userId, userName) for the label.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct CcOAuthCredential {
    pub access_token: String,
    pub user_id: Option<String>,
    pub user_name: Option<String>,
}

impl CcOAuthCredential {
    pub fn parse(kv: &str) -> Result<Self, String> {
        let val: serde_json::Value =
            serde_json::from_str(kv).map_err(|e| format!("CommandCode: invalid key_value JSON: {e}"))?;
        let access_token = val["access_token"]
            .as_str()
            .or_else(|| val["apiKey"].as_str())
            .or_else(|| val["api_key"].as_str())
            .ok_or_else(|| "CommandCode: missing access_token/apiKey".to_string())?
            .to_string();
        let user_id = val["user_id"]
            .as_str()
            .or_else(|| val["userId"].as_str())
            .map(String::from);
        let user_name = val["user_name"]
            .as_str()
            .or_else(|| val["userName"].as_str())
            .map(String::from);
        Ok(Self { access_token, user_id, user_name })
    }
}

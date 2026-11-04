use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use serde_json::json;

/// Custom error codes returned by NuvCode's `/v1/*` endpoints.
///
/// All errors follow the OpenAI-compatible shape:
/// ```json
/// { "error": { "message": "...", "type": "...", "code": "..." } }
/// ```
#[derive(Debug, Clone, thiserror::Error)]
pub enum GatewayError {
    // ---- 400 (Bad Request) ----
    #[error("Model must be prefixed with a provider ID (e.g. `mst/mistral-large`). Got: `{0}`")]
    InvalidModelFormat(String),

    #[error("Missing required field: `{0}`")]
    MissingField(String),

    #[error("Invalid JSON body: {0}")]
    InvalidJsonBody(String),

    #[error("Empty messages array — at least one message required")]
    EmptyMessages,

    // ---- 401 (Auth) ----
    #[error("Missing or invalid `Authorization` header. Send `Authorization: Bearer *** <key>` header.")]
    MissingAuthHeader,

    #[error("Invalid or inactive gateway API key")]
    InvalidApiKey,

    // ---- 403 (Forbidden) ----
    #[error("Provider `{0}` is currently disabled")]
    ProviderDisabled(String),

    // ---- 404 (Not Found) ----
    #[error("Provider not found: `{0}` — available providers: /v1/providers")]
    ProviderNotFound(String),

    #[error("Model `{model}` not found in provider `{provider}`")]
    ModelNotFound { provider: String, model: String },

    // ---- 429 (Rate Limit) ----
    #[error("All gateway keys for provider `{0}` are currently rate-limited. Try again in 60s.")]
    AllKeysRateLimited(String),

    #[error("Token limit reached: {used}/{max} tokens used")]
    TokenLimitExceeded { used: i64, max: i64 },

    // ---- 502/503/504 (Upstream issues) ----
    #[error("Upstream HTTP {status}: {body}")]
    ProviderHttpError { status: u16, body: String, provider: String, key_id: Option<String> },
    /// All API keys for provider `{0}` are exhausted or invalid
    #[error("All API keys for provider `{0}` are exhausted or invalid")]
    NoAvailableKeys(String),

    // ---- 500 (Internal) ----
    #[error("Internal error: {0}")]
    Internal(String),

    #[error("Upstream connection error: {0}")]
    ProviderError(String),
}

impl GatewayError {
    /// HTTP status + OpenAI-style `type` + custom `code`.
    /// `code` is the machine-readable identifier clients should switch on.
    pub fn parts(&self) -> (StatusCode, &'static str, &'static str) {
        match self {
            // 400
            Self::InvalidModelFormat(_) => (StatusCode::BAD_REQUEST, "invalid_request_error", "invalid_model_format"),
            Self::MissingField(_) => (StatusCode::BAD_REQUEST, "invalid_request_error", "missing_required_field"),
            Self::InvalidJsonBody(_) => (StatusCode::BAD_REQUEST, "invalid_request_error", "invalid_request_body"),
            Self::EmptyMessages => (StatusCode::BAD_REQUEST, "invalid_request_error", "empty_messages"),

            // 401
            Self::MissingAuthHeader => (StatusCode::UNAUTHORIZED, "authentication_error", "missing_authorization"),
            Self::InvalidApiKey => (StatusCode::UNAUTHORIZED, "authentication_error", "invalid_api_key"),

            // 403
            Self::ProviderDisabled(_) => (StatusCode::FORBIDDEN, "permission_error", "provider_disabled"),

            // 404
            Self::ProviderNotFound(_) => (StatusCode::NOT_FOUND, "not_found_error", "provider_not_found"),
            Self::ModelNotFound { .. } => (StatusCode::NOT_FOUND, "not_found_error", "model_not_found"),

            // 429
            Self::AllKeysRateLimited(_) => (StatusCode::TOO_MANY_REQUESTS, "rate_limit_error", "all_keys_rate_limited"),
            Self::TokenLimitExceeded { .. } => (StatusCode::TOO_MANY_REQUESTS, "token_limit_error", "token_limit_exceeded"),

            // 502/503/504
            Self::ProviderHttpError { status, .. } => (
                StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY),
                "upstream_error",
                "provider_http_error",
            ),
            Self::NoAvailableKeys(_) => (StatusCode::SERVICE_UNAVAILABLE, "service_unavailable_error", "no_available_keys"),

            // 500
            Self::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal_server_error", "internal_error"),
            Self::ProviderError(_) => (StatusCode::BAD_GATEWAY, "upstream_error", "provider_connection_error"),
        }
    }

    pub fn is_auth_error(&self) -> bool {
        matches!(self, Self::ProviderHttpError { status: 401, .. } | Self::ProviderHttpError { status: 403, .. })
    }

    pub fn is_rate_limit_error(&self) -> bool {
        matches!(self, Self::ProviderHttpError { status: 429, .. })
    }

    /// Extract the underlying provider_api_key_id when this error was tied
    /// to a specific key attempt (auth-build failure, upstream HTTP error,
    /// etc.). `None` for errors that aren't tied to a key.
    pub fn provider_api_key_id(&self) -> Option<&str> {
        match self {
            Self::ProviderHttpError { key_id: Some(id), .. } => Some(id.as_str()),
            _ => None,
        }
    }

    /// Extract HTTP status code for usage logging (independent of into_response formatting).
    pub fn http_status(&self) -> Option<u16> {
        match self {
            Self::ProviderHttpError { status, .. } => Some(*status),
            Self::NoAvailableKeys(_) => Some(503),
            Self::ProviderError(_) => Some(502),
            Self::AllKeysRateLimited(_) => Some(429),
            Self::TokenLimitExceeded { .. } => Some(429),
            Self::InvalidModelFormat(_) | Self::MissingField(_) | Self::InvalidJsonBody(_) | Self::EmptyMessages => Some(400),
            Self::MissingAuthHeader | Self::InvalidApiKey => Some(401),
            Self::ProviderDisabled(_) => Some(403),
            Self::ProviderNotFound(_) | Self::ModelNotFound { .. } => Some(404),
            Self::Internal(_) => Some(500),
        }
    }
}

/// Serialize-safe error body. Avoids `Serialize` derive on `thiserror` so
/// `provider` field may be `String` and `status` may be `u16`.
#[derive(Serialize)]
struct ErrorEnvelope<'a> {
    error: ErrorBody<'a>,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    message: String,
    #[serde(rename = "type")]
    kind: &'a str,
    code: String,
    param: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    suggestion: Option<String>,
}

impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        let (status, kind, code_prefix) = self.parts();

        let param = match &self {
            Self::MissingField(name) => Some(name.clone()),
            Self::InvalidModelFormat(m) => Some(format!("model=\"{}\"", m)),
            Self::ModelNotFound { model, .. } => Some(format!("model=\"{}\"", model)),
            _ => None,
        };

        let (message, code, suggestion) = match &self {
            Self::ProviderHttpError { status: s, body, provider, .. } => {
                let code = format!("provider_http_{}", s);
                let clean_msg = clean_upstream_error_body(body, provider);
                let suggestion = match s {
                    401 | 403 => Some(format!("Check that the API key for provider `{}` is active and valid in /admin/auth-files", provider)),
                    429 => Some(format!("Provider `{}` rate limited. Wait or add more keys in /admin/providers/{}", provider, provider)),
                    502 | 503 | 504 => Some(format!("Provider `{}` temporarily unavailable. Try again later.", provider)),
                    _ => None,
                };
                (clean_msg, code, suggestion)
            }
            Self::ProviderNotFound(p) => {
                (self.to_string(), code_prefix.to_string(), Some(format!("Available providers list: /v1/providers. Add `{}` via /admin/providers", p)))
            }
            Self::AllKeysRateLimited(p) => {
                (format!("All keys for provider `{}` are rate-limited. Retry after cooldown.", p), code_prefix.to_string(),
                 Some(format!("Wait 60s or add keys in /admin/providers/{}", p)))
            }
            Self::NoAvailableKeys(p) => {
                (format!("All keys for provider `{}` are exhausted or invalid.", p), code_prefix.to_string(),
                 Some(format!("Add active keys in /admin/providers/{}", p)))
            }
            Self::ProviderDisabled(p) => {
                (format!("Provider `{}` is disabled. Enable it in /admin/settings", p), code_prefix.to_string(), None)
            }
            _ => (self.to_string(), code_prefix.to_string(), None),
        };

        tracing::warn!(
            status = %status,
            code = %code,
            kind = %kind,
            error = %message,
            "request failed"
        );

        let body = ErrorEnvelope {
            error: ErrorBody {
                message,
                kind,
                code,
                param,
                suggestion,
            },
        };

        (status, Json(body)).into_response()
    }
}

fn clean_upstream_error_body(body: &str, provider: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return format!("Upstream provider `{}` returned an empty error response", provider);
    }

    // Try JSON: {"error":{"message":"..."}} or {"error":"..."}
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(trimmed) {
        // Recursively search for "message" field
        if let Some(msg) = extract_json_error_message(&parsed) {
            let truncated = if msg.len() > 200 { format!("{}...", &msg[..200]) } else { msg };
            return format!("[{}] {}", provider, truncated);
        }
        // Try "error" as string
        if let Some(msg) = parsed.get("error").and_then(|v| v.as_str()) {
            let truncated = if msg.len() > 200 { format!("{}...", &msg[..200]) } else { msg.to_string() };
            return format!("[{}] {}", provider, truncated);
        }
    }

    // Plain text or HTML — truncate to first 150 chars
    let cleaned = trimmed.replace('\n', " ").replace('\r', "");
    let cleaned = cleaned.chars().take(200).collect::<String>();
    if cleaned.contains("<!DOCTYPE") || cleaned.contains("<html") {
        return format!("[{}] Upstream returned an HTML error page (HTTP). Check provider status.", provider);
    }
    format!("[{}] {}", provider, cleaned)
}

fn extract_json_error_message(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Object(map) => {
            // Check direct "message" field
            if let Some(msg) = map.get("message").and_then(|v| v.as_str()) {
                return Some(msg.to_string());
            }
            // Check nested: error -> message
            if let Some(err) = map.get("error") {
                if let Some(msg) = err.get("message").and_then(|v| v.as_str()) {
                    return Some(msg.to_string());
                }
                if let Some(msg) = err.get("msg").and_then(|v| v.as_str()) {
                    return Some(msg.to_string());
                }
            }
            // Check "detail" (OpenAI-style error wrapper)
            if let Some(detail) = map.get("detail").and_then(|v| v.as_str()) {
                return Some(detail.to_string());
            }
            // Check nested objects
            for v in map.values() {
                if let Some(msg) = extract_json_error_message(v) {
                    return Some(msg);
                }
            }
            None
        }
        _ => None,
    }
}

/// Helper for clean inline json! responses in middleware / handlers that
/// don't want to construct a `GatewayError`.
pub fn err_response(status: StatusCode, kind: &str, code: &str, message: &str) -> Response {
    let body = json!({
        "error": {
            "message": message,
            "type": kind,
            "code": code,
        }
    });
    (status, Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_model_format_returns_400() {
        let e = GatewayError::InvalidModelFormat("foo".into());
        let (s, t, c) = e.parts();
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(t, "invalid_request_error");
        assert_eq!(c, "invalid_model_format");
    }

    #[test]
    fn missing_auth_header_is_401() {
        let e = GatewayError::MissingAuthHeader;
        let (s, _, c) = e.parts();
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        assert_eq!(c, "missing_authorization");
    }

    #[test]
    fn provider_http_code_uses_upstream_status() {
        let e = GatewayError::ProviderHttpError {
            status: 429,
            body: "rate-limited".into(),
            provider: "mst".into(),
            key_id: None,
        };
        let resp = e.into_response();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    }
}

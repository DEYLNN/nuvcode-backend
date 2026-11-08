use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::error::err_response;
use crate::state::AppState;
use std::sync::Arc;
use jsonwebtoken::{decode, DecodingKey, Validation, Algorithm};

/// Stored in request extensions by auth middleware for downstream handlers.
#[derive(Clone)]
pub struct GatewayKeyInfo {
    pub key_id: String,
    pub access_type: String,
    pub allowed_models: Vec<String>,
}

/// Auth middleware: protects /v1/* with gateway keys, protects /admin/api/* with JWT (if configured).
/// Public routes bypass.
pub async fn auth_middleware(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    request: Request<Body>,
    next: Next,
) -> impl IntoResponse {
    let path = request.uri().path().to_string();

    // CORS preflight — bypass auth entirely
    if request.method() == "OPTIONS" {
        return next.run(request).await;
    }

    // === Admin API JWT auth (if password configured) ===
    if path.starts_with("/admin/api/") {
        // Login endpoint is public (mounted at /admin/api/login in login.rs)
        if path.starts_with("/admin/api/login") {
            return next.run(request).await;
        }
        // Docs are public
        if path.starts_with("/admin/api/docs/") {
            return next.run(request).await;
        }

        let admin_password_set = state.config.auth.admin_password
            .as_ref()
            .map_or(false, |p| !p.is_empty());

        if !admin_password_set {
            // No password set — admin API is public
            return next.run(request).await;
        }

        let jwt_secret = state.config.auth.jwt_secret.clone()
            .unwrap_or_else(|| "default-dev-secret-change-in-prod".into());

        let auth_header = request
            .headers()
            .get("authorization")
            .and_then(|h| h.to_str().ok());

        let token = match auth_header {
            Some(h) if h.starts_with("Bearer ") => &h[7..],
            _ => return unauthorized_response("missing_token", "Missing admin token"),
        };

        let mut validation = Validation::new(Algorithm::HS256);
        validation.validate_exp = true;

        return match decode::<serde_json::Value>(
            token,
            &DecodingKey::from_secret(jwt_secret.as_bytes()),
            &validation,
        ) {
            Ok(_) => next.run(request).await,
            Err(_) => unauthorized_response("invalid_token", "Admin token invalid or expired"),
        }
    }

    // === Public paths (bypass completely) ===
    if path.starts_with("/health") || !path.starts_with("/v1/") {
        return next.run(request).await;
    }

    // === /v1/* Gateway key auth (existing logic) ===
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok());

    let key_value = match auth_header {
        Some(h) if h.starts_with("Bearer ") => &h[7..],
        _ => {
            return err_response(
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                "missing_authorization",
                "Missing or invalid `Authorization` header. Send `Authorization: Bearer <your-key>` header.",
            );
        }
    };

    let key_id_opt: Option<String> = sqlx::query_scalar(
        "SELECT id FROM gateway_keys WHERE key_value = $1"
    )
    .bind(key_value)
    .fetch_optional(&state.db)
    .await
    .unwrap_or(None);

    let valid: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM gateway_keys WHERE key_value = $1 AND is_active = true)"
    )
    .bind(key_value)
    .fetch_one(&state.db)
    .await
    .unwrap_or(false);

    if !valid {
        return err_response(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "invalid_api_key",
            "Invalid or inactive gateway API key",
        );
    }

    if let Some(kid) = &key_id_opt {
        let row = sqlx::query_as::<_, (String, String)>(
            "SELECT access_type, allowed_models FROM gateway_keys WHERE id = $1"
        )
        .bind(kid)
        .fetch_optional(&state.db)
        .await
        .unwrap_or(None);

        if let Some((access_type, allowed_models_str)) = row {
            let allowed_models: Vec<String> = if allowed_models_str.is_empty() {
                vec![]
            } else {
                serde_json::from_str(&allowed_models_str).unwrap_or_default()
            };
            let info = GatewayKeyInfo {
                key_id: kid.clone(),
                access_type,
                allowed_models,
            };
            let mut req = request;
            req.extensions_mut().insert(info);
            next.run(req).await
        } else {
            let mut req = request;
            req.extensions_mut().insert(GatewayKeyInfo {
                key_id: kid.clone(),
                access_type: "full".into(),
                allowed_models: vec![],
            });
            next.run(req).await
        }
    } else {
        next.run(request).await
    }
}

fn unauthorized_response(error: &'static str, message: &'static str) -> Response {
    err_response(
        StatusCode::UNAUTHORIZED,
        "authentication_error",
        error,
        message,
    )
}

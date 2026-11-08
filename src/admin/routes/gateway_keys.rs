use std::sync::Arc;

use axum::{
    extract::{Path, State},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::state::AppState;

#[derive(Serialize)]
pub struct GatewayKeyJson {
    pub id: String,
    pub key_value: String,
    pub label: Option<String>,
    /// `is_active` is a BOOLEAN column in Postgres. Stored as `i64` here (0/1)
    /// because the frontend expects a number, not a JSON boolean — the FE's
    /// `GatewayKeyJson` interface declares `is_active: number`.
    pub is_active: i64,
    pub access_type: String,
    pub allowed_models: Vec<String>,
    pub max_tokens: i64,
    pub created_at: String,
}

pub async fn api_list_keys(State(state): State<Arc<AppState>>) -> Json<Vec<GatewayKeyJson>> {
    let rows = sqlx::query_as::<_, (String, String, Option<String>, bool, String, String, i64, String)>(
        "SELECT id, key_value, label, is_active, access_type, allowed_models, max_tokens, created_at FROM gateway_keys ORDER BY created_at DESC"
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|(id, key_value, label, is_active, access_type, allowed_models_str, max_tokens, created_at)| {
        let allowed_models: Vec<String> = if allowed_models_str.is_empty() {
            vec![]
        } else {
            serde_json::from_str(&allowed_models_str).unwrap_or_default()
        };
        GatewayKeyJson { id, key_value, label, is_active: is_active as i64, access_type, allowed_models, max_tokens, created_at }
    })
    .collect();
    Json(rows)
}

#[derive(Deserialize)]
pub struct CreateKeyRequest {
    pub label: Option<String>,
    pub access_type: Option<String>,
    pub allowed_models: Option<Vec<String>>,
    pub max_tokens: Option<i64>,
}

#[derive(Serialize)]
pub struct CreateKeyResponse {
    pub success: bool,
    pub message: String,
    pub id: Option<String>,
    pub key_value: Option<String>,
}

pub async fn api_create_key(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateKeyRequest>,
) -> Json<CreateKeyResponse> {
    use uuid::Uuid;
    use rand::Rng;

    let id = format!("gk_{}", Uuid::new_v4().to_string().replace('-', "").chars().take(8).collect::<String>());
    let random_part: String = (0..64).map(|_| {
        let c: u8 = rand::thread_rng().gen_range(0..36);
        (if c < 26 { b'a' + c } else { b'0' + c - 26 }) as char
    }).collect();
    let key_value = format!("sk-axm-{}", random_part);

    let access_type = req.access_type.unwrap_or_else(|| "full".into());
    let allowed_models = serde_json::to_string(&req.allowed_models.unwrap_or_default()).unwrap_or_default();

    let max_tokens = req.max_tokens.unwrap_or(0);

    let result = sqlx::query(
        "INSERT INTO gateway_keys (id, key_value, label, access_type, allowed_models, max_tokens) VALUES ($1, $2, $3, $4, $5, $6)"
    )
    .bind(&id)
    .bind(&key_value)
    .bind(&req.label)
    .bind(&access_type)
    .bind(&allowed_models)
    .bind(max_tokens)
    .execute(&state.db)
    .await;

    match result {
        Ok(_) => Json(CreateKeyResponse {
            success: true,
            message: "Gateway key created".into(),
            id: Some(id),
            key_value: Some(key_value),
        }),
        Err(e) => Json(CreateKeyResponse {
            success: false,
            message: format!("Failed: {}", e),
            id: None,
            key_value: None,
        }),
    }
}

#[derive(Deserialize)]
pub struct UpdateKeyRequest {
    pub label: Option<String>,
    pub is_active: Option<bool>,
    pub access_type: Option<String>,
    pub allowed_models: Option<Vec<String>>,
    pub max_tokens: Option<i64>,
}

pub async fn api_update_key(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<UpdateKeyRequest>,
) -> Json<serde_json::Value> {
    if let Some(label) = &req.label {
        let _ = sqlx::query("UPDATE gateway_keys SET label = $1 WHERE id = $2")
            .bind(label).bind(&id).execute(&state.db).await;
    }
    if let Some(is_active) = req.is_active {
        let _ = sqlx::query("UPDATE gateway_keys SET is_active = $1 WHERE id = $2")
            .bind(is_active as i64).bind(&id).execute(&state.db).await;
    }
    if let Some(access_type) = &req.access_type {
        let _ = sqlx::query("UPDATE gateway_keys SET access_type = $1 WHERE id = $2")
            .bind(access_type).bind(&id).execute(&state.db).await;
    }
    if let Some(allowed_models) = &req.allowed_models {
        let json = serde_json::to_string(allowed_models).unwrap_or_default();
        let _ = sqlx::query("UPDATE gateway_keys SET allowed_models = $1 WHERE id = $2")
            .bind(&json).bind(&id).execute(&state.db).await;
    }
    if let Some(max_tokens) = req.max_tokens {
        let _ = sqlx::query("UPDATE gateway_keys SET max_tokens = $1 WHERE id = $2")
            .bind(max_tokens).bind(&id).execute(&state.db).await;
    }
    Json(serde_json::json!({"success": true}))
}

pub async fn api_delete_key(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let result = sqlx::query("DELETE FROM gateway_keys WHERE id = $1")
        .bind(&id)
        .execute(&state.db)
        .await;

    match result {
        Ok(_) => Json(serde_json::json!({"success": true, "message": "Deleted"})),
        Err(e) => Json(serde_json::json!({"success": false, "message": e.to_string()})),
    }
}

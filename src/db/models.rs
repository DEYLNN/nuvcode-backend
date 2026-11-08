use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ApiKey {
    pub id: String,
    pub provider_id: String,
    pub key_value: String,
    pub label: Option<String>,
    pub is_active: bool,
    pub last_used_at: Option<String>,
    pub consecutive_use_count: i64,
    pub consecutive_error_count: i64,
    pub created_at: String,
    pub updated_at: String,
}

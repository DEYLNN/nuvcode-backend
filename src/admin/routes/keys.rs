use std::sync::Arc;

use axum::{extract::{Query, State}, Json};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::state::AppState;

#[derive(Deserialize)]
pub struct KeysQuery {
    pub page: Option<i64>,
    pub per_page: Option<i64>,
    pub provider_id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_bool")]
    pub only_problem: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_optional_bool")]
    pub only_disabled: Option<bool>,
    #[serde(default)]
    pub status_code: Option<String>,
    #[serde(default)]
    pub query: Option<String>,
}

/// Lenient bool parser — accepts "1"/"0"/"true"/"false"/"yes"/"no" as bool.
/// `Option<bool>` default serde only accepts "true"/"false" — that's too strict
/// for query strings. FE often sends "1" / "0" as truthy shorthand.
fn deserialize_optional_bool<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where D: serde::Deserializer<'de> {
    use serde::de::Error;
    let s: Option<String> = Option::deserialize(deserializer)?;
    Ok(s.map(|v| {
        let v = v.to_ascii_lowercase();
        v == "1" || v == "true" || v == "yes"
    }))
}

#[derive(Serialize)]
pub struct KeyListItem {
    pub id: String,
    pub provider_id: String,
    pub label: Option<String>,
    pub key_type: String,
    pub is_active: bool,
    pub key_preview: String,
    pub key_value: String,
    pub locked_until: Option<String>,
    pub last_error_status: Option<i64>,
    pub last_error_message: Option<String>,
    pub last_error_at: Option<String>,
    pub backoff_level: i64,
    pub consecutive_error_count: i64,
    pub created_at: String,
    pub email: String,
    pub plan: String,
    pub account_id: String,
    pub has_refresh: bool,
    pub has_access: bool,
    pub expires_at: String,
}

#[derive(Serialize)]
pub struct KeysListResponse {
    pub keys: Vec<KeyListItem>,
    pub total: i64,
    pub page: i64,
    pub per_page: i64,
}

pub async fn api_list_keys(
    State(state): State<Arc<AppState>>,
    Query(q): Query<KeysQuery>,
) -> Json<KeysListResponse> {
    let page = q.page.unwrap_or(1).max(1);
    let per_page = q.per_page.unwrap_or(100).clamp(1, 200);
    let offset = (page - 1) * per_page;

    // Build dynamic WHERE — PostgreSQL uses $N numbered placeholders.
    let mut where_sql = String::from(" WHERE 1=1");
    let mut bind_idx = 1usize;
    if q.provider_id.is_some() {
        where_sql.push_str(&format!(" AND provider_id = ${bind_idx}"));
        bind_idx += 1;
    }
    if q.only_disabled.unwrap_or(false) {
        where_sql.push_str(" AND is_active = false");
    }
    if q.only_problem.unwrap_or(false) {
        where_sql.push_str(" AND (last_error_status IS NOT NULL OR last_error_message IS NOT NULL)");
    }
    let status_code_filter: Option<i64> = q.status_code.as_ref().and_then(|s| s.parse::<i64>().ok());
    if status_code_filter.is_some() {
        where_sql.push_str(&format!(" AND last_error_status = ${bind_idx}"));
        bind_idx += 1;
    }
    // Text search — matches provider_id, label, or key_type. Used by the
    // Auth Files search box. Like-wrapped so partial matches work.
    let query_filter = q.query.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()).map(|s| format!("%{s}%"));
    if query_filter.is_some() {
        where_sql.push_str(&format!(" AND (provider_id LIKE ${bind_idx} OR label LIKE ${bind_idx} OR COALESCE(key_type,'') LIKE ${bind_idx})"));
        bind_idx += 1;
    }

    let count_sql = format!("SELECT COUNT(*) FROM api_keys{}", where_sql);
    let list_sql = format!(
        "SELECT id, provider_id, label, COALESCE(key_type, 'apikey'), key_value, is_active, locked_until, last_error_status, last_error_message, last_error_at, backoff_level, consecutive_error_count, created_at FROM api_keys{} ORDER BY provider_id, created_at DESC LIMIT ${bind_idx} OFFSET ${offset_idx}",
        where_sql,
        offset_idx = bind_idx + 1
    );

    // Count with binds
    let mut count_q = sqlx::query_scalar::<_, i64>(&count_sql);
    if let Some(pid) = &q.provider_id { count_q = count_q.bind(pid); }
    if let Some(sc) = status_code_filter { count_q = count_q.bind(sc); }
    if let Some(qf) = &query_filter { count_q = count_q.bind(qf).bind(qf).bind(qf); }
    let total: i64 = count_q.fetch_one(&state.db).await.unwrap_or(0);

    // List with binds + LIMIT/OFFSET
    let mut list_q = sqlx::query_as::<_, (String, String, Option<String>, String, String, bool, Option<String>, Option<i64>, Option<String>, Option<String>, i64, i64, String)>(&list_sql);
    if let Some(pid) = &q.provider_id { list_q = list_q.bind(pid); }
    if let Some(sc) = status_code_filter { list_q = list_q.bind(sc); }
    if let Some(qf) = &query_filter { list_q = list_q.bind(qf).bind(qf).bind(qf); }
    let rows = list_q
        .bind(per_page)
        .bind(offset)
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();

    let keys = rows.into_iter().map(|(id, provider_id, label, key_type, key_value, is_active, locked_until, last_error_status, last_error_message, last_error_at, backoff_level, consecutive_error_count, created_at)| {
        let preview = if key_value.len() > 12 {
            format!("{}...{}", &key_value[..6], &key_value[key_value.len() - 4..])
        } else {
            key_value.clone()
        };
        let oauth: serde_json::Value = serde_json::from_str(&key_value).unwrap_or_default();
        let expires_at = oauth
            .get("expires_at")
            .or_else(|| oauth.get("expiresAt"))
            .map(|v| v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string()))
            .unwrap_or_default();
        let email = oauth.get("email").and_then(|v| v.as_str()).unwrap_or_default().to_owned();
        let plan = oauth.get("plan").and_then(|v| v.as_str()).unwrap_or_default().to_owned();
        let account_id = oauth
            .get("account_id")
            .or_else(|| oauth.get("accountId"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned();
        KeyListItem {
            id, provider_id, label, key_type, is_active,
            key_preview: preview, key_value,
            locked_until, last_error_status, last_error_message, last_error_at, backoff_level, consecutive_error_count, created_at,
            email, plan, account_id,
            has_refresh: oauth.get("refresh_token").or_else(|| oauth.get("refreshToken")).and_then(|v| v.as_str()).is_some_and(|v| !v.is_empty()),
            has_access: oauth.get("access_token").or_else(|| oauth.get("accessToken")).and_then(|v| v.as_str()).is_some_and(|v| !v.is_empty()),
            expires_at,
        }
    }).collect();

    Json(KeysListResponse { keys, total, page, per_page })
}

#[derive(Deserialize)]
pub struct DedupeRequest {
    /// Restrict dedupe to one provider; None = scan all.
    pub provider_id: Option<String>,
}

#[derive(Serialize)]
pub struct DedupeResponse {
    pub removed: i64,
    pub kept: i64,
    pub groups: i64,
}

/// Deduplicate API keys — keep oldest per (provider_id, key_value).
/// OAuth entries are left alone (key_type='oauth') since they hold tokens
/// the user reconnected manually, not actual API keys.
pub async fn api_dedupe_keys(
    State(state): State<Arc<AppState>>,
    Json(req): Json<DedupeRequest>,
) -> Json<DedupeResponse> {
    // Find duplicates — keep min(created_at), remove the rest.
    // OAuth rows are excluded from both target + keeper selection.
    let base_query = "SELECT provider_id, key_value, COUNT(*) AS n, MIN(created_at) \
         FROM api_keys \
         WHERE COALESCE(key_type, 'apikey') != 'oauth' \
         GROUP BY provider_id, key_value \
         HAVING COUNT(*) > 1 \
         ORDER BY provider_id, key_value";
    let dup_rows: Vec<(String, String, i64, String)> = if let Some(pid) = req.provider_id.as_deref() {
        sqlx::query_as::<_, (String, String, i64, String)>(
            &format!("{base_query} AND provider_id = $1")
        )
        .bind(pid)
        .fetch_all(&state.db)
        .await
        .unwrap_or_default()
    } else {
        sqlx::query_as::<_, (String, String, i64, String)>(base_query)
            .fetch_all(&state.db)
            .await
            .unwrap_or_default()
    };

    let groups = dup_rows.len() as i64;
    let mut removed: i64 = 0;
    let mut kept: i64 = 0;

    for (pid, kv, n, min_created) in &dup_rows {
        kept += 1; // 1 per group kept
        removed += n - 1; // n-1 deletions per group
        // Delete everything for this (provider_id, key_value) row EXCEPT the
        // row with the oldest created_at. This keeps behavior predictable
        // even if rowids surprise us.
        let r = sqlx::query(
            "DELETE FROM api_keys \
             WHERE provider_id = $1 AND key_value = $2 \
               AND COALESCE(key_type, 'apikey') != 'oauth' \
               AND created_at != $3"
        )
        .bind(pid)
        .bind(kv)
        .bind(min_created)
        .execute(&state.db)
        .await
        .map(|x| x.rows_affected() as i64)
        .unwrap_or(0);
        let _ = r;
    }

    Json(DedupeResponse { removed, kept, groups })
}

#[derive(Serialize)]
pub struct KeysStatsResponse {
    pub total: i64,
    pub active: i64,
    pub disabled: i64,
    pub providers: Vec<ProviderKeyCount>,
    /// Number of distinct (provider_id, key_value) groups with >1 non-OAuth
    /// entries. This is the count of "duplicate groups" — running dedupe
    /// would delete (sum of group sizes) - duplicates_count rows.
    pub duplicates: i64,
}

#[derive(Serialize)]
pub struct ProviderKeyCount {
    pub provider_id: String,
    pub count: i64,
    pub active: i64,
}

/// Full key stats for FE provider dropdown — single GROUP BY scan,
/// independent of pagination. Same filter knobs as api_list_keys so FE
/// can show counts that match the visible list.
pub async fn api_keys_stats(
    State(state): State<Arc<AppState>>,
    Query(q): Query<KeysQuery>,
) -> Json<KeysStatsResponse> {
    use crate::db::count_keys_per_provider;
    let all = count_keys_per_provider(&state.db).await;

    // Apply same filters as api_list_keys so counts reflect the same set.
    let rows = sqlx::query_as::<_, (i64, i64)>(
        "SELECT \
           COUNT(*), \
           SUM(CASE WHEN is_active = true THEN 1 ELSE 0 END) \
         FROM api_keys WHERE 1=1 \
           AND ($1 IS NULL OR provider_id = $1) \
           AND ($2 = false OR is_active = false) \
           AND ($3 = false OR (last_error_status IS NOT NULL OR last_error_message IS NOT NULL))"
    )
    .bind(&q.provider_id)
    .bind(q.only_disabled.unwrap_or(false))
    .bind(q.only_problem.unwrap_or(false))
    .fetch_one(&state.db)
    .await
    .unwrap_or((0, 0));

    let total = rows.0;
    let active = rows.1;
    let duplicates = crate::db::count_duplicate_groups(&state.db, q.provider_id.as_deref()).await;
    Json(KeysStatsResponse {
        total,
        active,
        disabled: total - active,
        providers: all.into_iter().map(|(pid, c, a)| ProviderKeyCount {
            provider_id: pid, count: c, active: a,
        }).collect(),
        duplicates,
    })
}

#[derive(Deserialize)]
pub struct AddKeyRequest {
    pub provider_id: String,
    pub key_value: String,
    pub label: Option<String>,
    pub key_type: Option<String>,
}

#[derive(Serialize)]
pub struct AddKeyResponse {
    pub success: bool,
    pub message: String,
}

pub async fn api_add_key(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AddKeyRequest>,
) -> Json<AddKeyResponse> {
    let id = format!("key_{}", &Uuid::new_v4().to_string()[..8]);
    let label = req.label.unwrap_or_default();
    let key_type = req.key_type.as_deref().unwrap_or("apikey");

    // Reject duplicates — same (provider_id, key_value) already exists.
    // OAuth tokens (stored under key_type='oauth' by the OAuth flow) are
    // intentionally excluded from this check — users reconnect manually.
    let dup: Option<String> = sqlx::query_scalar(
        "SELECT id FROM api_keys \
         WHERE provider_id = $1 AND key_value = $2 \
           AND COALESCE(key_type, 'apikey') != 'oauth' LIMIT 1"
    )
    .bind(&req.provider_id)
    .bind(&req.key_value)
    .fetch_optional(&state.db)
    .await
    .unwrap_or(None);

    if dup.is_some() {
        return Json(AddKeyResponse {
            success: false,
            message: format!("Duplicate key already exists for provider '{}'", req.provider_id),
        });
    }

    let result = sqlx::query(
        "INSERT INTO api_keys (id, provider_id, key_value, label, is_active, key_type) VALUES ($1, $2, $3, $4, true, $5)",
    )
    .bind(&id)
    .bind(&req.provider_id)
    .bind(&req.key_value)
    .bind(&label)
    .bind(key_type)
    .execute(&state.db)
    .await;

    match result {
        Ok(_) => {
            // Reload provider to pick up new key
            let mut pm = state.provider_manager.write().await;
            let _ = pm.reload_provider(&req.provider_id).await;
            drop(pm);

            // Auto-disable all models ONLY on the first key (0 → 1 transition).
            // Subsequent keys must NOT clobber the admin's manual toggle state.
            // Each new model must be explicitly enabled via /admin/api/models/toggle.
            let existing_key_count = crate::db::count_provider_keys(&state.db, &req.provider_id, true).await;
            if existing_key_count <= 1 {
                let pm = state.provider_manager.read().await;
                let models = pm.list_all_models_unfiltered().await;
                // Use the provider's actual model-prefix (may differ from the
                // DB row id for custom providers — e.g. `nx` vs `custom_nx`).
                let prefix = pm.get(&req.provider_id)
                    .and_then(|p| p.metadata().model_prefix)
                    .map(|pf| format!("{}/", pf))
                    .unwrap_or_else(|| format!("{}/", req.provider_id));
                let _ = pm;
                for m in models {
                    if m.id.starts_with(&prefix) {
                        let _ = sqlx::query(
                            "INSERT INTO disabled_models (model_id) VALUES ($1)",
                        )
                        .bind(&m.id)
                        .execute(&state.db)
                        .await;
                    }
                }
            }

            Json(AddKeyResponse {
                success: true,
                message: format!(
                    "Key {} added. Models auto-disabled; toggle them in Models page.",
                    id
                ),
            })
        }
        Err(e) => Json(AddKeyResponse {
            success: false,
            message: format!("Failed: {}", e),
        }),
    }
}

#[derive(Deserialize)]
pub struct DeleteKeyRequest {
    pub provider_id: Option<String>,
    pub key_id: String,
}

pub async fn api_delete_key(
    State(state): State<Arc<AppState>>,
    Json(req): Json<DeleteKeyRequest>,
) -> Json<AddKeyResponse> {
    // Get provider_id from the key we're about to delete
    let provider_id: Option<String> = if let Some(pid) = &req.provider_id {
        Some(pid.clone())
    } else {
        sqlx::query_scalar("SELECT provider_id FROM api_keys WHERE id=$1")
            .bind(&req.key_id)
            .fetch_optional(&state.db)
            .await
            .unwrap_or(None)
    };

    let result = sqlx::query("DELETE FROM api_keys WHERE id=$1")
        .bind(&req.key_id)
        .execute(&state.db)
        .await;

    match result {
        Ok(_) => {
            // Reload provider
            if let Some(pid) = &provider_id {
                let mut pm = state.provider_manager.write().await;
                let _ = pm.reload_provider(pid).await;
            }
            Json(AddKeyResponse {
                success: true,
                message: "Key deleted".into(),
            })
        }
        Err(e) => Json(AddKeyResponse {
            success: false,
            message: format!("Failed: {}", e),
        }),
    }
}

#[derive(Deserialize)]
pub struct ToggleKeyRequest {
    pub key_id: String,
    pub is_active: bool,
}

/// Toggle a key active/inactive. Re-enabling resets the error counter,
/// disabling persists to DB and to KeyManager in-memory state.
pub async fn api_toggle_key(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ToggleKeyRequest>,
) -> Json<AddKeyResponse> {
    // Update DB
    let result = sqlx::query("UPDATE api_keys SET is_active=$1 WHERE id=$2")
        .bind(req.is_active)
        .bind(&req.key_id)
        .execute(&state.db)
        .await;

    if let Err(e) = result {
        return Json(AddKeyResponse {
            success: false,
            message: format!("Failed: {}", e),
        });
    }

    // Sync in-memory KeyManager state — must reload provider to pick up
    // the new is_active for in-flight requests. Reload pulls fresh keys.
    let provider_id: Option<String> = sqlx::query_scalar("SELECT provider_id FROM api_keys WHERE id=$1")
        .bind(&req.key_id)
        .fetch_optional(&state.db)
        .await
        .unwrap_or(None);

    // Also reset DB-side consecutive_error_count when manually re-enabled.
    if req.is_active {
        let _ = sqlx::query("UPDATE api_keys SET consecutive_error_count = 0, locked_until = NULL, last_error_status = NULL, last_error_message = NULL, last_error_at = NULL, backoff_level = 0 WHERE id = $1")
            .bind(&req.key_id)
            .execute(&state.db)
            .await;
    }

    if let Some(pid) = provider_id {
        let mut pm = state.provider_manager.write().await;
        let _ = pm.reload_provider(&pid).await;
        // Also reset in-memory error state when manually re-enabled.
        if req.is_active {
            let _ = pm.reset_key_error_state(&req.key_id).await;
        }
    }

    Json(AddKeyResponse {
        success: true,
        message: if req.is_active { "Key enabled".into() } else { "Key disabled".into() },
    })
}

#[derive(Deserialize)]
pub struct BulkAddKeysRequest {
    pub provider_id: String,
    pub keys: Vec<String>,
    pub label: Option<String>,
}

#[derive(Serialize)]
pub struct BulkAddKeysResponse {
    pub added: usize,
    pub duplicates: usize,
    pub total: usize,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Bulk-add API keys for a single provider. Skips duplicates silently.
/// Guards: provider must exist, category must be apikey, keys must be non-empty strings.
pub async fn api_bulk_add_keys(
    State(state): State<Arc<AppState>>,
    Json(req): Json<BulkAddKeysRequest>,
) -> Json<BulkAddKeysResponse> {
    // Guard 1: keys must be non-empty
    let keys: Vec<&str> = req.keys.iter().map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
    if keys.is_empty() {
        return Json(BulkAddKeysResponse {
            added: 0, duplicates: 0, total: 0,
            message: "No valid keys provided".into(),
            error: Some("keys array is empty or contains only whitespace".into()),
        });
    }

    // Guard 2: provider must exist and be apikey
    let pm_check = state.provider_manager.read().await;
    let category = pm_check
        .get(&req.provider_id)
        .map(|p| p.metadata().category.clone());
    drop(pm_check);

    match &category {
        None => {
            return Json(BulkAddKeysResponse {
                added: 0, duplicates: 0, total: 0,
                message: format!("Provider '{}' not found", req.provider_id),
                error: Some("unknown provider_id".into()),
            });
        }
        Some(cat) if cat != "apikey" => {
            return Json(BulkAddKeysResponse {
                added: 0, duplicates: 0, total: 0,
                message: format!("Provider '{}' is not apikey (category: {})", req.provider_id, cat),
                error: Some("only apikey providers allowed".into()),
            });
        }
        _ => {}
    }

    let label = req.label.unwrap_or_default();
    let mut added = 0usize;
    let mut duplicates = 0usize;

    for key_value in &keys {

        // Check duplicate
        let dup: Option<String> = sqlx::query_scalar(
            "SELECT id FROM api_keys \
             WHERE provider_id = $1 AND key_value = $2 \
               AND COALESCE(key_type, 'apikey') != 'oauth' LIMIT 1"
        )
        .bind(&req.provider_id)
        .bind(*key_value)
        .fetch_optional(&state.db)
        .await
        .unwrap_or(None);

        if dup.is_some() {
            duplicates += 1;
            continue;
        }

        let id = format!("key_{}", &Uuid::new_v4().to_string()[..8]);
        let r = sqlx::query(
            "INSERT INTO api_keys (id, provider_id, key_value, label, is_active, key_type) VALUES ($1, $2, $3, $4, true, $5)",
        )
        .bind(&id)
        .bind(&req.provider_id)
        .bind(*key_value)
        .bind(&label)
        .bind("apikey")
        .execute(&state.db)
        .await;

        if r.is_ok() {
            added += 1;
        }
    }

    // Reload provider to pick up new keys
    if added > 0 {
        let mut pm = state.provider_manager.write().await;
        let _ = pm.reload_provider(&req.provider_id).await;

        // Auto-disable models on first key (0 → N transition)
        let existing_key_count = crate::db::count_provider_keys(&state.db, &req.provider_id, true).await;
        let was_zero = existing_key_count as usize <= added; // only new keys added this batch
        if was_zero {
            let models = pm.list_all_models_unfiltered().await;
            let prefix = pm.get(&req.provider_id)
                .and_then(|p| p.metadata().model_prefix)
                .map(|pf| format!("{}/", pf))
                .unwrap_or_else(|| format!("{}/", req.provider_id));
            drop(pm);
            for m in models {
                if m.id.starts_with(&prefix) {
                    let _ = sqlx::query(
                        "INSERT INTO disabled_models (model_id) VALUES ($1)",
                    )
                    .bind(&m.id)
                    .execute(&state.db)
                    .await;
                }
            }
        }
    }

    Json(BulkAddKeysResponse {
        added,
        duplicates,
        total: added + duplicates,
        message: format!("{} added, {} duplicates skipped", added, duplicates),
        error: None,
    })
}

#[derive(Deserialize)]
pub struct BulkEnableRequest {
    pub key_ids: Vec<String>,
}

/// Bulk-enable selected keys — same semantics as api_toggle_key(true):
/// activates key AND resets error state (backoff, consecutive errors,
/// last error fields). Used by Auth Files "Enable" bulk action.
pub async fn api_bulk_enable_keys(
    State(state): State<Arc<AppState>>,
    Json(req): Json<BulkEnableRequest>,
) -> Json<serde_json::Value> {
    let mut enabled = 0usize;
    let mut failed = 0usize;
    let mut provider_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    for key_id in &req.key_ids {
        // 1. Activate + reset error state in one UPDATE (only if currently disabled)
        let r = sqlx::query(
            "UPDATE api_keys SET is_active=true, consecutive_error_count=0, locked_until=NULL, \
             last_error_status=NULL, last_error_message=NULL, last_error_at=NULL, backoff_level=0 \
             WHERE id=$1 AND is_active=false"
        )
        .bind(key_id)
        .execute(&state.db)
        .await;

        match r {
            Ok(res) if res.rows_affected() > 0 => {
                enabled += 1;
                if let Ok(Some(pid)) = sqlx::query_scalar::<_, String>(
                    "SELECT provider_id FROM api_keys WHERE id=$1"
                ).bind(key_id).fetch_optional(&state.db).await {
                    provider_ids.insert(pid);
                }
            }
            _ => failed += 1,
        }
    }

    // Reload affected providers so KeyManager picks up fresh is_active state.
    for pid in &provider_ids {
        let mut pm = state.provider_manager.write().await;
        let _ = pm.reload_provider(pid).await;
    }

    Json(serde_json::json!({
        "success": true,
        "enabled": enabled,
        "failed": failed,
        "message": format!("Enabled {enabled} key(s){}{}", if failed > 0 { format!(", {failed} already active/failed") } else { String::new() }, if failed > 0 { "" } else { "" }),
    }))
}

// --- Key count per provider (apikey only) ---

#[derive(Deserialize)]
pub struct CountQuery {
    pub provider_id: String,
}

#[derive(Serialize)]
pub struct CountResponse {
    pub provider_id: String,
    pub total: i64,
    pub active: i64,
    pub disabled: i64,
}

pub async fn api_count_keys(
    State(state): State<Arc<AppState>>,
    Query(q): Query<CountQuery>,
) -> Json<CountResponse> {
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM api_keys WHERE provider_id = $1")
        .bind(&q.provider_id)
        .fetch_one(&state.db)
        .await
        .unwrap_or(0);
    let active: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM api_keys WHERE provider_id = $1 AND is_active = true")
        .bind(&q.provider_id)
        .fetch_one(&state.db)
        .await
        .unwrap_or(0);
    Json(CountResponse {
        provider_id: q.provider_id,
        total,
        active,
        disabled: total - active,
    })
}

// --- Bulk delete keys ---

#[derive(Deserialize)]
pub struct BulkDeleteRequest {
    pub provider_id: String,
    /// "all" = delete all keys, otherwise delete by key_value match
    pub action: String,
    #[serde(default)]
    pub key_value: Option<String>,
}

#[derive(Serialize)]
pub struct BulkDeleteResponse {
    pub deleted: usize,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub async fn api_bulk_delete_keys(
    State(state): State<Arc<AppState>>,
    Json(req): Json<BulkDeleteRequest>,
) -> Json<BulkDeleteResponse> {
    // Guard: apikey provider only
    let pm_check = state.provider_manager.read().await;
    let category = pm_check
        .get(&req.provider_id)
        .map(|p| p.metadata().category.clone());
    drop(pm_check);

    match &category {
        None => {
            return Json(BulkDeleteResponse {
                deleted: 0,
                message: format!("Provider '{}' not found", req.provider_id),
                error: Some("unknown provider_id".into()),
            });
        }
        Some(cat) if cat != "apikey" => {
            return Json(BulkDeleteResponse {
                deleted: 0,
                message: format!("Provider '{}' is not apikey", req.provider_id),
                error: Some("only apikey providers allowed".into()),
            });
        }
        _ => {}
    }

    let result = if req.action == "all" {
        sqlx::query("DELETE FROM api_keys WHERE provider_id = $1")
            .bind(&req.provider_id)
            .execute(&state.db)
            .await
    } else if let Some(kv) = &req.key_value {
        sqlx::query("DELETE FROM api_keys WHERE provider_id = $1 AND key_value = $2")
            .bind(&req.provider_id)
            .bind(kv)
            .execute(&state.db)
            .await
    } else {
        return Json(BulkDeleteResponse {
            deleted: 0,
            message: "action must be 'all' or provide key_value".into(),
            error: Some("invalid action".into()),
        });
    };

    match result {
        Ok(r) => {
            let deleted = r.rows_affected() as usize;
            if deleted > 0 {
                let mut pm = state.provider_manager.write().await;
                let _ = pm.reload_provider(&req.provider_id).await;
            }
            Json(BulkDeleteResponse {
                deleted,
                message: format!("Deleted {} key(s) from {}", deleted, req.provider_id),
                error: None,
            })
        }
        Err(e) => Json(BulkDeleteResponse {
            deleted: 0,
            message: format!("Failed: {}", e),
            error: Some("db error".into()),
        }),
    }
}

// --- Serve docs ---

pub async fn serve_key_injection_docs() -> impl axum::response::IntoResponse {
    let content = include_str!("../../../docs/KEY_INJECTION.md");
    axum::response::Response::builder()
        .header("content-type", "text/markdown; charset=utf-8")
        .body(axum::body::Body::from(content.to_string()))
        .unwrap()
}

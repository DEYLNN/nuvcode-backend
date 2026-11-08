use std::sync::Arc;

use axum::Router;

use crate::state::AppState;
use super::routes;

pub fn admin_routes(state: Arc<AppState>) -> Router {
    Router::new()
        .merge(providers_routes(state.clone()))
        .merge(custom_providers_routes(state.clone()))
        .merge(settings_routes(state.clone()))
        .merge(database_routes(state.clone()))
        .merge(gateway_routes(state.clone()))
        .merge(keys_routes(state.clone()))
        .merge(models_routes(state.clone()))
        .merge(usage_routes(state.clone()))
        .merge(combos_routes(state.clone()))
        .merge(quota_routes(state.clone()))
        .merge(routes::unsloth::unsloth_routes(state.clone()))
}
use axum::routing::{get, post, patch, delete};

fn providers_routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/admin/api/providers", get(routes::providers::api_providers))
        .route("/admin/api/providers/:id", get(routes::providers::api_provider_detail))
        .route("/admin/api/providers/:id/validate-models", get(routes::providers::api_validate_models))
        .route("/admin/api/providers/:id/test", post(routes::providers::api_test_model))
        .route("/admin/api/providers/:id/block", post(routes::providers::api_block_model))
        .route("/admin/api/providers/:id/unblock", post(routes::providers::api_unblock_model))
        .route("/admin/api/providers/:id/custom-models", get(routes::providers::custom_models::api_list_custom_models))
        .route("/admin/api/providers/:id/custom-models", post(routes::providers::custom_models::api_add_custom_model_for_provider))
        .route("/admin/api/providers/:id/custom-models/:model_id", delete(routes::providers::custom_models::api_remove_custom_model_for_provider))
        .with_state(state)
}

fn custom_providers_routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/admin/api/custom-providers", get(routes::custom_providers::api_list_custom_providers))
        .route("/admin/api/custom-providers", post(routes::custom_providers::api_create_custom_provider))
        .route("/admin/api/custom-providers/:id", get(routes::custom_providers::api_get_custom_provider))
        .route("/admin/api/custom-providers/:id", delete(routes::custom_providers::api_delete_custom_provider))
        .route("/admin/api/custom-providers/:id/models", post(routes::custom_providers::api_add_custom_model))
        .route("/admin/api/custom-providers/:id/models/:model_id", delete(routes::custom_providers::api_remove_custom_model))
        .with_state(state)
}

fn settings_routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/admin/api/settings", get(routes::settings::api_settings))
        .route("/admin/api/settings/toggle", post(routes::settings::api_toggle_setting))
        .with_state(state)
}

fn database_routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/admin/api/database", get(routes::database::api_database_info))
        .route("/admin/api/database/export", get(routes::database::api_database_export))
        .route("/admin/api/database/import", post(routes::database::api_database_import))
        .with_state(state)
}

fn gateway_routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/admin/api/gateway_keys", get(routes::gateway_keys::api_list_keys))
        .route("/admin/api/gateway_keys", post(routes::gateway_keys::api_create_key))
        .route("/admin/api/gateway_keys/:id", delete(routes::gateway_keys::api_delete_key))
        .route("/admin/api/gateway_keys/:id", patch(routes::gateway_keys::api_update_key))
        .with_state(state)
}

fn keys_routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/admin/api/keys", get(routes::keys::api_list_keys))
        .route("/admin/api/keys/stats", get(routes::keys::api_keys_stats))
        .route("/admin/api/keys", post(routes::keys::api_add_key))
        .route("/admin/api/keys/delete", post(routes::keys::api_delete_key))
        .route("/admin/api/keys/toggle", post(routes::keys::api_toggle_key))
        .route("/admin/api/keys/bulk-add", post(routes::keys::api_bulk_add_keys))
        .route("/admin/api/keys/bulk-delete", post(routes::keys::api_bulk_delete_keys))
        .route("/admin/api/keys/bulk-enable", post(routes::keys::api_bulk_enable_keys))
        .route("/admin/api/keys/count", get(routes::keys::api_count_keys))
        .route("/admin/api/keys/dedupe", post(routes::keys::api_dedupe_keys))
        .with_state(state)
}

fn models_routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/admin/api/models/toggle", post(routes::models::api_toggle_model))
        .route("/admin/api/models/disabled", get(routes::models::api_disabled_models))
        .route("/admin/api/models/all", get(routes::models::api_all_models))
        .route("/admin/api/models/blocked", get(routes::models::api_blocked_models))
        .route("/admin/api/models/bulk-toggle", post(routes::models::api_bulk_toggle))
        .with_state(state)
}

fn usage_routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/admin/api/usage/stats", get(routes::usage::api_usage_stats))
        .route("/admin/api/usage/stats/keys", get(routes::usage::api_usage_keys))
        .route("/admin/api/usage/by-provider", get(routes::usage::api_usage_by_provider))
        .route("/admin/api/usage/by-model", get(routes::usage::api_usage_by_model))
        .route("/admin/api/usage/today", get(routes::usage::api_usage_today))
        .route("/admin/api/usage/latency", get(routes::usage::api_usage_latency))
        .route("/admin/api/logs", get(routes::usage::api_usage_logs))
        .route("/admin/api/logs/clear", post(routes::usage::api_clear_logs))
        .route("/admin/api/usage/stream", get(routes::usage::api_usage_stream))
        .with_state(state)
}

fn quota_routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/admin/api/quota/providers", get(routes::quota::api_quota_providers))
        .route("/admin/api/quota/keys", get(routes::quota::api_oauth_keys))
        .route("/admin/api/quota/:key_id", get(routes::quota::api_usage_quota))
        .route("/admin/api/quota/refresh/:key_id", post(routes::quota::api_refresh_token))
        .with_state(state)
}

fn combos_routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/admin/api/combos", get(routes::combos::api_list_combos))
        .route("/admin/api/combos", post(routes::combos::api_create_combo))
        .route("/admin/api/combos/:id", post(routes::combos::api_update_combo))
        .route("/admin/api/combos/:id", delete(routes::combos::api_delete_combo))
        .route("/admin/api/combos/:id/toggle", post(routes::combos::api_toggle_combo))
        .with_state(state)
}

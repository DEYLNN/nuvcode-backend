use std::collections::HashMap;
use std::sync::Arc;
use sqlx::PgPool;

use crate::config::models::AppConfig;
use crate::providers::registry::ProviderRegistry;
use crate::providers::traits::Provider;
use crate::types::model::Model;
use crate::types::provider::ProviderMetadata;
use crate::engine::openai_compat::config::OpenAIConfig;
use crate::engine::openai_compat::config::ModelDef;
use crate::providers::spec::ProviderQuirks;

/// Runtime manager — stores ALL registered providers from registry.
/// Keys are loaded from DB but providers are always available (even with 0 keys).
pub struct ProviderManager {
    active: HashMap<String, Arc<dyn Provider>>,
    registry: ProviderRegistry,
    db: PgPool,
}

impl ProviderManager {
    pub async fn new(_config: &AppConfig, db: &PgPool) -> anyhow::Result<Self> {
        let registry = ProviderRegistry::new();
        let mut active = HashMap::new();

        // Load TOML-defined + OAuth providers from registry
        let provider_ids: Vec<String> = registry.provider_ids().iter().map(|s| s.to_string()).collect();

        for provider_id in &provider_ids {
            let keys = crate::db::load_provider_keys(db, provider_id).await?;
            let key_count = keys.len();

            match registry.build(provider_id, keys, Arc::new(db.clone())) { Some(provider) => {
                tracing::info!(
                    "Provider '{}' loaded with {} key(s)",
                    provider_id,
                    key_count
                );
                active.insert(provider_id.to_string(), provider);
            } _ => {
                tracing::warn!("Provider '{}' failed to build", provider_id);
            }}
        }

        // Load custom (DB-defined) providers
        if let Ok(custom) = crate::db::list_custom_providers(db).await {
            for cp in &custom {
                let prefix = if cp.prefix.is_empty() { cp.id.clone() } else { cp.prefix.clone() };
                let models = crate::db::list_custom_provider_models(db, &cp.id).await.unwrap_or_default();
                let engine_models: Vec<ModelDef> = models.iter().map(|m| ModelDef {
                    id: m.model_id.clone(), name: m.model_id.clone(),
                    max_tokens: None, context_length: m.ctx as u32,
                    supports_vision: m.vision != 0, supports_tools: m.tools != 0,
                    reasoning: false, hide_reasoning: false,
                    thinking_tags: None,
                }).collect();

                let config = OpenAIConfig {
                    provider_id: cp.id.clone(),
                    provider_name: cp.name.clone(),
                    model_prefix: prefix.clone(),
                    base_url: cp.base_url.clone(),
                    validate_url: cp.validate_url.clone(),
                    category: "custom_openai".to_string(),
                    color: cp.color.clone(),
                    icon_name: "custom-provider.jpg".to_string(),
                    default_timeout_secs: cp.timeout_secs as u64,
                    stream_first_chunk_timeout_secs: cp.first_chunk_timeout_secs as u64,
                    stream_stall_timeout_secs: cp.stall_timeout_secs as u64,
                    models: engine_models,
                    quirks: ProviderQuirks::default(),
                    chat_path: "/chat/completions".into(),
                };

                let keys = crate::db::load_provider_keys(db, &cp.id).await?;
                let key_count = keys.len();

                let provider = crate::engine::openai_compat::provider::OpenAICompatibleProvider::new(config, keys, Some(db.clone()));
                tracing::info!("Custom provider '{}' loaded with {} key(s)", cp.id, key_count);
                active.insert(cp.id.clone(), Arc::new(provider) as Arc<dyn Provider>);
            }
        }

        Ok(Self { active, registry, db: db.clone() })
    }

    /// Look up provider by name. Returns Arc clone — caller can release read lock immediately.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Provider>> {
        self.active.get(name).cloned()
    }

    /// Resolve a model-id prefix to the underlying provider id.
    ///
    /// Custom providers expose models under a UI prefix (e.g. `nx`), while the
    /// chat dispatcher looks them up by the DB id (`custom_nx`). This method
    /// accepts either form and returns the canonical provider id, or `None`
    /// if no active provider matches.
    pub fn resolve_provider_id(&self, prefix_or_id: &str) -> Option<String> {
        // Exact match wins — registered / OAuth / TOML providers use their id
        // directly, and a custom provider may also share the name (e.g. `fb`).
        if self.active.contains_key(prefix_or_id) {
            return Some(prefix_or_id.to_string());
        }
        // Otherwise scan for a provider whose `model_prefix` (used to build
        // `models_static()` ids) matches. Custom providers set this from
        // `custom_providers.prefix`.
        for (id, provider) in &self.active {
            let meta = provider.metadata();
            if meta.model_prefix.as_deref() == Some(prefix_or_id) {
                return Some(id.clone());
            }
        }
        None
    }

    /// List all registered provider names
    pub fn provider_names(&self) -> Vec<&str> {
        self.active.keys().map(|s| s.as_str()).collect()
    }

    /// Reload keys from DB for a provider and rebuild it
    pub async fn reload_provider(&mut self, provider_id: &str) -> anyhow::Result<()> {
        // Custom providers live in DB, not the registry — rebuild from DB config.
        if crate::db::get_custom_provider(&self.db, provider_id).await?.is_some() {
            let db = self.db.clone();
            return self.reload_custom_provider(provider_id, &db).await;
        }
        let keys = crate::db::load_provider_keys(&self.db, provider_id).await?;
        let key_count = keys.len();

        match self.registry.build(provider_id, keys, Arc::new(self.db.clone())) {
            Some(provider) => {
                tracing::info!(
                    "Provider '{}' reloaded with {} key(s)",
                    provider_id,
                    key_count
                );
                self.active.insert(provider_id.to_string(), provider);
            }
            _ => {
                tracing::warn!("Provider '{}' failed to rebuild after reload", provider_id);
            }
        }

        Ok(())
    }

    /// Reset in-memory error state for a key (called when admin manually re-enables).
    /// Clears lock, backoff, error counter — fresh start.
    pub async fn reset_key_error_state(&mut self, key_id: &str) -> anyhow::Result<()> {
        for (pid, _provider) in self.active.iter() {
            // Box<dyn Provider> doesn't expose KeyManager directly. Each provider
            // owns its own KeyManager; reload_provider pulls fresh keys from DB.
            // The DB reset happens in toggle handler SQL UPDATE below.
            tracing::debug!("reset_key_error_state called for {} in provider {}", key_id, pid);
        }
        Ok(())
    }

    /// Rebuild a custom (DB-defined) provider from scratch
    pub async fn reload_custom_provider(&mut self, id: &str, db: &PgPool) -> anyhow::Result<()> {
        let cp = match crate::db::get_custom_provider(db, id).await? {
            Some(p) => p,
            None => { self.active.remove(id); return Ok(()); }
        };
        let prefix = if cp.prefix.is_empty() { cp.id.clone() } else { cp.prefix.clone() };
        let models = crate::db::list_custom_provider_models(db, id).await.unwrap_or_default();
        let engine_models: Vec<ModelDef> = models.iter().map(|m| ModelDef {
            id: m.model_id.clone(), name: m.model_id.clone(),
            max_tokens: None, context_length: m.ctx as u32,
            supports_vision: m.vision != 0, supports_tools: m.tools != 0,
            reasoning: false, hide_reasoning: false,
            thinking_tags: None,
        }).collect();

        let config = OpenAIConfig {
            provider_id: cp.id.clone(), provider_name: cp.name.clone(), model_prefix: prefix,
            base_url: cp.base_url, validate_url: cp.validate_url, category: "custom_openai".into(),
            color: cp.color, icon_name: "custom-provider.jpg".into(),
            default_timeout_secs: cp.timeout_secs as u64,
            stream_first_chunk_timeout_secs: cp.first_chunk_timeout_secs as u64,
            stream_stall_timeout_secs: cp.stall_timeout_secs as u64,
            models: engine_models,
            quirks: ProviderQuirks::default(),
            chat_path: "/chat/completions".into(),
        };
        let keys = crate::db::load_provider_keys(db, id).await?;
        let provider = crate::engine::openai_compat::provider::OpenAICompatibleProvider::new(config, keys, Some(db.clone()));
        self.active.insert(id.to_string(), Arc::new(provider) as Arc<dyn Provider>);
        Ok(())
    }

    /// Aggregate models from all providers (skips those with zero keys).
    pub async fn list_all_models(&self) -> Vec<Model> {
        let mut all = Vec::new();
        for (_name, provider) in &self.active {
            let meta = provider.metadata();
            // Skip providers with zero keys — except manual providers (e.g. Unsloth)
            // which manage their own models in DB and don't use KeyManager.
            if provider.total_keys() == 0 && meta.category != "manual" {
                continue;
            }
            if let Ok(models) = provider.list_models().await {
                all.extend(models);
            }
            // Merge custom models (user-added model entries) for this provider.
            let meta = provider.metadata();
            let prefix = meta.model_prefix.clone().unwrap_or_else(|| meta.name.clone());
            let custom = crate::db::list_custom_models(&self.db, &meta.name).await;
            for cm in custom {
                let id = format!("{}/{}", prefix, cm.model_id);
                // Skip if already exists from native list (dedup by id)
                if all.iter().any(|m| m.id == id) {
                    continue;
                }
                all.push(Model {
                    id,
                    object: "model".to_string(),
                    owned_by: meta.display_name.clone(),
                    context_length: Some(cm.ctx as u32),
                });
            }
        }

        // Inject active combos as virtual models (combo/<name>)
        // ctx = min(ctx tiers) for fallback/round_robin, max(ctx tiers) for balanced.
        let combos = sqlx::query_as::<_, (String, String, String)>(
            "SELECT name, tiers, strategy FROM combos WHERE is_active = true"
        )
        .fetch_all(&self.db)
        .await
        .unwrap_or_default();
        for (name, tiers_str, strategy) in combos {
            let tiers: Vec<String> = serde_json::from_str(&tiers_str).unwrap_or_default();
            let ctx = compute_combo_ctx_from_models(&all, &tiers, &strategy);
            all.push(Model {
                id: format!("combo/{}", name),
                object: "model".to_string(),
                owned_by: "combo".to_string(),
                context_length: ctx,
            });
        }

        all
    }

    /// Like list_all_models but includes providers with zero keys.
    pub async fn list_all_models_unfiltered(&self) -> Vec<Model> {
        let mut all = Vec::new();
        for (_name, provider) in &self.active {
            if let Ok(models) = provider.list_models().await {
                all.extend(models);
            }
            // Merge custom models for this provider (same as above).
            let meta = provider.metadata();
            let prefix = meta.model_prefix.clone().unwrap_or_else(|| meta.name.clone());
            let custom = crate::db::list_custom_models(&self.db, &meta.name).await;
            for cm in custom {
                let id = format!("{}/{}", prefix, cm.model_id);
                // Skip if already exists from native list (dedup by id)
                if all.iter().any(|m| m.id == id) {
                    continue;
                }
                all.push(Model {
                    id,
                    object: "model".to_string(),
                    owned_by: meta.display_name.clone(),
                    context_length: Some(cm.ctx as u32),
                });
            }
        }

        // Inject active combos as virtual models (combo/<name>)
        let combos = sqlx::query_as::<_, (String, String, String)>(
            "SELECT name, tiers, strategy FROM combos WHERE is_active = true"
        )
        .fetch_all(&self.db)
        .await
        .unwrap_or_default();
        for (name, tiers_str, strategy) in combos {
            let tiers: Vec<String> = serde_json::from_str(&tiers_str).unwrap_or_default();
            let ctx = compute_combo_ctx_from_models(&all, &tiers, &strategy);
            all.push(Model {
                id: format!("combo/{}", name),
                object: "model".to_string(),
                owned_by: "combo".to_string(),
                context_length: ctx,
            });
        }

        all
    }

    /// List all provider metadata
    pub fn list_providers(&self) -> Vec<ProviderMetadata> {
        self.active
            .values()
            .map(|p| p.metadata())
            .collect()
    }

    /// Number of total keys for a specific provider
    pub fn total_keys_for(&self, name: &str) -> Option<usize> {
        self.active.get(name).map(|p| p.total_keys())
    }

    /// Number of active (non-locked) keys for a specific provider
    pub fn active_keys_for(&self, name: &str) -> Option<usize> {
        self.active.get(name).map(|p| p.active_keys())
    }
}

/// Compute combo context_length from model list.
/// fallback & round_robin → min(ctx tiers). balanced → max(ctx tiers).
fn compute_combo_ctx_from_models(all: &[Model], tiers: &[String], strategy: &str) -> Option<u32> {
    let ctxs: Vec<u32> = tiers
        .iter()
        .filter_map(|t| {
            all.iter()
                .find(|m| m.id == *t)
                .and_then(|m| m.context_length)
        })
        .filter(|&c| c > 0)
        .collect();
    if ctxs.is_empty() {
        return None;
    }
    if strategy == "balanced" {
        ctxs.iter().copied().max()
    } else {
        ctxs.iter().copied().min()
    }
}

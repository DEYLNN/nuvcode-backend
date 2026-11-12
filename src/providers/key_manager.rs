use std::collections::HashMap;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use sqlx::PgPool;

use crate::db::models::ApiKey;
use crate::error::GatewayError;
use crate::services::key_strategy::{strategy_from_config, KeyStrategy, StrategyCtx};

/// Number of consecutive retryable errors before a key is auto-deactivated
/// (set is_active=false). Reset to 0 on mark_success. Admin can re-enable via FE toggle.
pub const AUTO_DEACTIVATE_THRESHOLD: i64 = 3;

/// Runtime state for each key — extends the DB `ApiKey` with in-memory tracking.
#[derive(Debug, Clone)]
pub struct KeyState {
    pub key: ApiKey,
    /// Instant when this key was locked (account-level).
    pub locked_at: Option<Instant>,
    /// Explicit account-level lock expiry.
    pub locked_until: Option<Instant>,
    /// Exponential backoff level (rate-limit only).
    pub backoff_level: u32,
    /// Consecutive sticky-round-robin uses.
    pub consecutive_use_count: i64,
    /// Consecutive retryable errors — auto-deactivate when threshold reached.
    pub consecutive_error_count: i64,
    /// Last time this key was picked.
    pub last_used_at: Option<Instant>,
    /// Per-model lock expiries: (model_id -> expiry Instant).
    pub model_locks: HashMap<String, Instant>,
    /// Reason for the lock (human-readable).
    pub lock_reason: String,
}

impl KeyState {
    /// Check if account-level lock is active.
    pub fn is_locked(&self, now: Instant) -> bool {
        match self.locked_until {
            Some(expiry) => now < expiry,
            None => false,
        }
    }

    /// Check if a specific model is locked on this key.
    pub fn is_model_locked(&self, model: &str, now: Instant) -> bool {
        self.model_locks
            .get(model)
            .map(|expiry| now < *expiry)
            .unwrap_or(false)
    }
}

/// Lock classification config — mirrors 9router's ERROR_RULES.
pub struct KeyLockConfig {
    /// Auth errors (401/403): fixed cooldown.
    pub auth_cooldown_secs: u64,
    /// Rate limit (429): exponential backoff base (seconds).
    pub rate_limit_backoff_base: u64,
    /// Rate limit: max backoff (seconds).
    pub rate_limit_backoff_max: u64,
    /// Transient errors (5xx): fixed cooldown.
    pub transient_cooldown_secs: u64,
}

impl Default for KeyLockConfig {
    fn default() -> Self {
        Self {
            auth_cooldown_secs: 120,
            rate_limit_backoff_base: 90,
            rate_limit_backoff_max: 300,
            transient_cooldown_secs: 120,
        }
    }
}

/// Multi-key manager with pluggable selection strategy, account-level lock,
/// per-model lock, exponential backoff, and fallback loop support.
///
/// Provider-agnostic. Each provider instance holds one `KeyManager`.
pub struct KeyManager {
    /// All key states, in provider-defined order (typically priority-sorted).
    states: Mutex<Vec<KeyState>>,
    /// Fallback cursor for non-sticky strategies.
    cursor: AtomicUsize,
    /// Lock cooldown configuration.
    config: KeyLockConfig,
    /// Pluggable key selection strategy.
    strategy: Box<dyn KeyStrategy>,
    /// Provider ID for display.
    provider_id: String,
    /// DB pool for persisting lock state across restarts.
    db_pool: Option<PgPool>,
}

impl KeyManager {
    pub fn new(keys: Vec<ApiKey>, provider_id: &str) -> Self {
        Self::new_with_pool(keys, provider_id, None)
    }

    pub fn new_with_pool(keys: Vec<ApiKey>, provider_id: &str, pool: Option<PgPool>) -> Self {
        let states: Vec<KeyState> = keys
            .into_iter()
            .map(|key| KeyState {
                key,
                locked_at: None,
                locked_until: None,
                backoff_level: 0,
                consecutive_use_count: 0,
                consecutive_error_count: 0,
                last_used_at: None,
                model_locks: HashMap::new(),
                lock_reason: String::new(),
            })
            .collect();
        Self {
            states: Mutex::new(states),
            cursor: AtomicUsize::new(0),
            config: KeyLockConfig::default(),
            strategy: strategy_from_config("round-robin", 1),
            provider_id: provider_id.to_string(),
            db_pool: pool,
        }
    }

    pub fn with_strategy(mut self, strategy: Box<dyn KeyStrategy>) -> Self {
        self.strategy = strategy;
        self
    }

    pub fn with_config(mut self, config: KeyLockConfig) -> Self {
        self.config = config;
        self
    }

    /// Collect garbage-expired locks (both account and model).
    fn gc(&self, now: Instant) {
        if let Ok(mut states) = self.states.lock() {
            for ks in states.iter_mut() {
                // Account-level lock GC
                if let Some(expiry) = ks.locked_until {
                    if now >= expiry {
                        ks.locked_at = None;
                        ks.locked_until = None;
                        ks.backoff_level = 0;
                        ks.lock_reason.clear();
                    }
                }
                // Model-level lock GC
                ks.model_locks.retain(|_, expiry| now < *expiry);
            }
        }
    }

    /// Pick the next available key using the configured strategy.
    pub fn next(&self) -> Result<ApiKey, GatewayError> {
        self.next_for_model(None)
    }

    /// Pick next key for a specific model (checks model locks).
    pub fn next_for_model(&self, model: Option<&str>) -> Result<ApiKey, GatewayError> {
        self.next_excluding(model, &[])
    }

    /// Pick next key while excluding specific key IDs (for fallback loop).
    pub fn next_excluding(
        &self,
        model: Option<&str>,
        exclude_ids: &[String],
    ) -> Result<ApiKey, GatewayError> {
        let now = Instant::now();
        self.gc(now);

        let selected_id = {
            let states = self.states.lock().unwrap();
            let ctx = StrategyCtx {
                keys: &states,
                exclude_ids,
                model,
                now,
            };
            self.strategy.select(&ctx).map(|ks| ks.key.id.clone())
        };

        let Some(selected_id) = selected_id else {
            return Err(GatewayError::NoAvailableKeys(
                "All keys locked or unavailable".into(),
            ));
        };

        let mut states = self.states.lock().unwrap();
        let selected = states
            .iter_mut()
            .find(|ks| ks.key.id == selected_id)
            .ok_or_else(|| GatewayError::NoAvailableKeys("Selected key disappeared".into()))?;
        selected.last_used_at = Some(now);
        selected.consecutive_use_count += 1;
        let key = selected.key.clone();
        drop(selected);
        for ks in states.iter_mut().filter(|ks| ks.key.id != selected_id) {
            ks.consecutive_use_count = 0;
        }
        Ok(key)
    }

    /// Mark success on a key: unlock, reset backoff, reset consecutive count.
    /// Also persists the recovery to DB so the key appears active after restart.
    pub fn mark_success(&self, key_id: &str) {
        if let Ok(mut states) = self.states.lock() {
            if let Some(ks) = states.iter_mut().find(|s| s.key.id == key_id) {
                ks.locked_at = None;
                ks.locked_until = None;
                ks.backoff_level = 0;
                ks.consecutive_error_count = 0;  // reset on success
                ks.lock_reason.clear();
                // Clear ONLY expired + current-model model locks.
                // (9router clears just the current model + expired ones on success.)
                let now = Instant::now();
                ks.model_locks.retain(|_, expiry| now < *expiry);
            }
        }
        // Persist to DB: clear lock state, mark last_error_at as "recovered now"
        if let Some(pool) = &self.db_pool {
            let pool = pool.clone();
            let kid = key_id.to_string();
            let now_iso = chrono::Utc::now().to_rfc3339();
            tokio::spawn(async move {
                let _ = sqlx::query(
                    "UPDATE api_keys SET locked_until = NULL, last_error_status = NULL, \
                     last_error_message = NULL, last_error_at = $2, \
                     backoff_level = 0 WHERE id = $1"
                )
                .bind(&kid)
                .bind(&now_iso)
                .execute(&pool)
                .await;
            });
        }
    }

    /// Lock a key after error (account-level). Calls `lock_key_for_model` with `None`.
    pub fn lock_key(&self, key_id: &str, status: u16, reason: String) {
        self.lock_key_for_model(key_id, None, status, reason);
    }

    /// Lock a key for a specific model after error.
    /// `model=None` means account-level lock (all models).
    /// `model=Some(...)` means only that model is locked on this key.
    pub fn lock_key_for_model(&self, key_id: &str, model: Option<&str>, status: u16, reason: String) {
        let now = Instant::now();
        let mut states = self.states.lock().unwrap();

        // Compute cooldown
        let (cooldown_secs, new_backoff) = match status {
            401 | 403 => (self.config.auth_cooldown_secs, 0u32),
            429 => {
                let current_level = states
                    .iter()
                    .find(|s| s.key.id == key_id)
                    .map(|s| s.backoff_level)
                    .unwrap_or(0);
                let level = current_level.min(15);
                let cooldown = (self.config.rate_limit_backoff_base as u64)
                    .saturating_mul(1u64 << level)
                    .min(self.config.rate_limit_backoff_max);
                (cooldown, level + 1)
            }
            _ => (self.config.transient_cooldown_secs, 0u32),
        };

        let expiry = now
            .checked_add(Duration::from_secs(cooldown_secs))
            .unwrap_or(now + Duration::from_secs(30));

        let readable = if let Ok(v) = serde_json::from_str::<serde_json::Value>(&reason) {
            v["error"]["message"]
                .as_str()
                .map(|s| s.to_string())
                .or_else(|| v["error"].as_str().map(|s| s.to_string()))
                .or_else(|| v["message"].as_str().map(|s| s.to_string()))
                .unwrap_or_else(|| reason.split('{').next().unwrap_or(&reason).trim().to_string())
        } else {
            reason.split('{').next().unwrap_or(&reason).trim().to_string()
        };

        if let Some(ks) = states.iter_mut().find(|s| s.key.id == key_id) {
            if let Some(model_id) = model {
                // Per-model lock
                ks.model_locks.insert(model_id.to_string(), expiry);
            } else {
                // Account-level lock — increment error counter
                ks.locked_at = Some(now);
                ks.locked_until = Some(expiry);
                ks.backoff_level = new_backoff;
                ks.consecutive_error_count += 1;
                ks.lock_reason = format!(
                    "HTTP {} — {} (cooldown {}s, backoff_level={}, errors={})",
                    status, readable, cooldown_secs, new_backoff, ks.consecutive_error_count
                );
            }
        }

        // Auto-deactivate if consecutive errors exceed threshold
        let auto_deactivated = {
            let mut deactivate = false;
            if model.is_none() {
                if let Some(ks) = states.iter().find(|s| s.key.id == key_id) {
                    if ks.consecutive_error_count >= AUTO_DEACTIVATE_THRESHOLD {
                        deactivate = true;
                    }
                }
            }
            if deactivate {
                if let Some(ks) = states.iter_mut().find(|s| s.key.id == key_id) {
                    ks.key.is_active = false;
                }
            }
            deactivate
        };

        let target = model.unwrap_or("<account>");
        tracing::warn!(
            "Key '{}' locked for {} on '{}' for {}s (backoff={}){}",
            key_id,
            target,
            status,
            cooldown_secs,
            new_backoff,
            if auto_deactivated { " — AUTO-DEACTIVATED" } else { "" }
        );

        // Persist to DB
        let now_iso = chrono::Utc::now().to_rfc3339();
        let expires_iso = if cooldown_secs > 0 {
            let dur = chrono::Duration::seconds(cooldown_secs as i64);
            Some((chrono::Utc::now() + dur).to_rfc3339())
        } else {
            None
        };
        if let Some(pool) = &self.db_pool {
            let pool = pool.clone();
            let kid = key_id.to_string();
            let lock_until = expires_iso;
            let st = status as i64;
            let msg = readable.clone();
            let bl = new_backoff as i64;
            let will_deactivate = auto_deactivated;
            tokio::spawn(async move {
                let _ = sqlx::query(
                    "UPDATE api_keys SET locked_until = $1, last_error_status = $2, \
                     last_error_message = $3, last_error_at = $4, \
                     backoff_level = $5, consecutive_error_count = CASE WHEN $6 THEN consecutive_error_count + 1 ELSE consecutive_error_count END, \
                     is_active = CASE WHEN $7 THEN false ELSE is_active END \
                     WHERE id = $8"
                )
                .bind(&lock_until)
                .bind(st)
                .bind(&msg)
                .bind(&now_iso)
                .bind(bl)
                .bind(true)  // always bump on lock (in-memory already incremented)
                .bind(will_deactivate)
                .bind(&kid)
                .execute(&pool)
                .await;
            });
        }
    }

    /// Get active count (keys not locked at account level).
    pub fn active_count(&self) -> usize {
        let now = Instant::now();
        let states = self.states.lock().unwrap();
        states
            .iter()
            .filter(|s| !s.is_locked(now))
            .count()
    }

    /// Total keys.
    pub fn total_count(&self) -> usize {
        self.states.lock().unwrap().len()
    }

    /// Manually toggle key active state. Called from FE /keys/:id/toggle endpoint.
    /// `true` = enable (active), `false` = disable. Also resets the error counter
    /// when re-enabling, since admin override is treated as a fresh start.
    pub fn set_active(&self, key_id: &str, active: bool) -> Result<(), GatewayError> {
        let mut states = self.states.lock().unwrap();
        let ks = states.iter_mut().find(|s| s.key.id == key_id)
            .ok_or_else(|| GatewayError::ProviderError(format!("Key not found: {}", key_id)))?;
        ks.key.is_active = active;
        if active {
            // Reset error state on manual re-enable.
            ks.consecutive_error_count = 0;
            ks.locked_at = None;
            ks.locked_until = None;
            ks.backoff_level = 0;
            ks.lock_reason.clear();
            ks.model_locks.clear();
        }
        Ok(())
    }

    /// Key IDs managed.
    pub fn key_ids(&self) -> Vec<String> {
        self.states
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.key.id.clone())
            .collect()
    }

    /// All currently locked keys with remaining cooldown info.
    pub fn locked_keys(&self) -> Vec<(String, u64, String)> {
        let now = Instant::now();
        let states = self.states.lock().unwrap();
        states
            .iter()
            .filter_map(|s| {
                let remaining = match s.locked_until {
                    Some(expiry) if expiry > now => expiry.duration_since(now).as_secs(),
                    _ => return None,
                };
                Some((s.key.id.clone(), remaining, s.lock_reason.clone()))
            })
            .collect()
    }
}

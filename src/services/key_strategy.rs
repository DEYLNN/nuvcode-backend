use crate::providers::key_manager::KeyState;

/// Context passed to strategy's `select` — current key states + model filter.
pub struct StrategyCtx<'a> {
    /// All keys managed by this KeyManager, in creation order.
    pub keys: &'a [KeyState],
    /// IDs to exclude (e.g. keys that just failed in this request).
    pub exclude_ids: &'a [String],
    /// Model to check model-level locks against.
    pub model: Option<&'a str>,
    /// Current timestamp (instant) for lock expiry checks.
    pub now: std::time::Instant,
}

/// A key selection strategy.
///
/// - `FillFirst`: pick first available (priority-sorted). Default.
/// - `RoundRobin`: sticky round-robin with consecutive-use tracking.
pub trait KeyStrategy: Send + Sync {
    fn select<'a>(&self, ctx: &StrategyCtx<'a>) -> Option<&'a KeyState>;
    fn box_clone(&self) -> Box<dyn KeyStrategy>;
}

// ── FillFirst ──────────────────────────────────────────────

#[derive(Clone, Default)]
pub struct FillFirst;

impl KeyStrategy for FillFirst {
    fn select<'a>(&self, ctx: &StrategyCtx<'a>) -> Option<&'a KeyState> {
        ctx.keys.iter().find(|k| {
            if !k.key.is_active { return false; }       // skip deactivated
            if ctx.exclude_ids.contains(&k.key.id) {
                return false;
            }
            if k.is_locked(ctx.now) {
                return false;
            }
            if let Some(model) = ctx.model {
                if k.is_model_locked(model, ctx.now) {
                    return false;
                }
            }
            true
        })
    }

    fn box_clone(&self) -> Box<dyn KeyStrategy> {
        Box::new(self.clone())
    }
}

// ── RoundRobin (Sticky) ─────────────────────────────────────

#[derive(Clone)]
pub struct RoundRobin {
    /// How many consecutive uses on the same key before rotating.
    pub sticky_limit: u32,
}

impl RoundRobin {
    pub fn new(sticky_limit: u32) -> Self {
        Self { sticky_limit }
    }
}

impl KeyStrategy for RoundRobin {
    fn select<'a>(&self, ctx: &StrategyCtx<'a>) -> Option<&'a KeyState> {
        // Filter available: not excluded, not locked, not model-locked
        let available: Vec<&KeyState> = ctx.keys.iter().filter(|k| {
            if !k.key.is_active { return false; }       // skip deactivated
            if ctx.exclude_ids.contains(&k.key.id) {
                return false;
            }
            if k.is_locked(ctx.now) {
                return false;
            }
            if let Some(model) = ctx.model {
                if k.is_model_locked(model, ctx.now) {
                    return false;
                }
            }
            true
        }).collect();

        if available.is_empty() {
            return None;
        }
        if available.len() == 1 {
            return Some(available[0]);
        }

        // Sticky: sort by last_used_at (most recent first)
        let mut sorted = available.clone();
        sorted.sort_by(|a, b| {
            b.last_used_at.unwrap_or(std::time::Instant::now())
                .cmp(&a.last_used_at.unwrap_or(std::time::Instant::now()))
        });

        let current = sorted[0];
        let count = current.consecutive_use_count;

        if current.last_used_at.is_some() && count < self.sticky_limit as i64 {
            // Stay with current — sticky
            Some(current)
        } else {
            // Rotate to least recently used
            let lru = sorted.iter()
                .min_by_key(|k| k.last_used_at.unwrap_or(std::time::Instant::now()));
            lru.copied()
        }
    }

    fn box_clone(&self) -> Box<dyn KeyStrategy> {
        Box::new(self.clone())
    }
}

/// Helper to build the default strategy from a config string.
pub fn strategy_from_config(name: &str, sticky_limit: u32) -> Box<dyn KeyStrategy> {
    match name {
        "fill-first" => Box::new(FillFirst),
        _ => Box::new(RoundRobin::new(sticky_limit)),
    }
}

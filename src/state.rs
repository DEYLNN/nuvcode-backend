use sqlx::postgres::PgPool;
use std::sync::atomic::AtomicI64;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

use crate::config::models::AppConfig;
use crate::providers::manager::ProviderManager;
use crate::services::usage_tracking::UsageTracker;

/// Channel capacity for SSE `/admin/api/usage/stream` subscribers. New
/// rows in the `usage` table fire a notification here. 64 keeps the
/// broadcast ring bounded without dropping events under typical load.
pub type UsageEvent = crate::db::UsageLogRow;
pub type UsageBroadcast = broadcast::Sender<UsageEvent>;

pub struct AppState {
    pub config: AppConfig,
    pub db: PgPool,
    pub provider_manager: Arc<RwLock<ProviderManager>>,
    pub usage_tracker: Arc<UsageTracker>,
    /// Sender side of the usage broadcast channel. The tracker publishes
    /// after every successful or failed insert; admins subscribe via
    /// `GET /admin/api/usage/stream` (SSE) for push updates.
    pub usage_broadcast: UsageBroadcast,
    pub public_ip: String,
    pub public_url: String,
    /// Real-time count of requests currently being processed (in-flight).
    /// Incremented by logging middleware on entry, decremented on exit.
    pub in_flight: AtomicI64,
}

impl AppState {
    pub async fn new(config: AppConfig, db: PgPool) -> anyhow::Result<Self> {
        let provider_manager = Arc::new(RwLock::new(ProviderManager::new(&config, &db).await?));
        // Wire the usage broadcast BEFORE building the tracker so every
        // save() can publish to SSE subscribers without extra plumbing.
        let (usage_broadcast, _rx_initial) = broadcast::channel::<UsageEvent>(64);
        let usage_tracker = Arc::new(
            UsageTracker::new(db.clone()).with_broadcast(usage_broadcast.clone()),
        );
        let public_ip = crate::utils::detect_public_ip().await;
        tracing::info!("Detected public IP: {}", public_ip);
        let public_url = config.server.public_url.clone()
            .unwrap_or_else(|| format!("http://{}:{}", public_ip, config.server.port));
        tracing::info!("Public URL: {}", public_url);
        Ok(Self {
            config,
            db,
            provider_manager,
            usage_tracker,
            usage_broadcast,
            public_ip,
            public_url,
            in_flight: AtomicI64::new(0),
        })
    }
}
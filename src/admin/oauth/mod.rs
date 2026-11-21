use std::sync::Arc;
use axum::routing::{get, post};
use axum::Router;
use crate::state::AppState;

mod gb;
mod kc;
mod cbai;
mod cx;
mod cc;

pub fn routes(state: Arc<AppState>) -> Router {
    Router::new()
        // KiloCode OAuth
        .route("/admin/api/oauth/kc/start", get(kc::start))
        .route("/admin/api/oauth/kc/poll", post(kc::poll))
        .route("/admin/api/oauth/cbai/start", get(cbai::start))
        .route("/admin/api/oauth/cbai/poll", post(cbai::poll))
        .route("/admin/api/oauth/cbai/refresh/:key_id", post(cbai::refresh))
        // Grok Build OAuth
        .route("/admin/api/oauth/gb/start", get(gb::start))
        .route("/admin/api/oauth/gb/poll", post(gb::poll))
        .route("/admin/api/oauth/gb/refresh/:key_id", post(gb::refresh))
        // OpenAI Codex OAuth — authorization_code + PKCE
        .route("/admin/api/oauth/cx/start", get(cx::start))
        .route("/admin/api/oauth/cx/callback", get(cx::exchange))
        .route("/admin/api/oauth/cx/exchange", post(cx::manual))
        .route("/admin/api/oauth/cx/manual", post(cx::manual))
        // Command Code OAuth — browser hand-off
        .route("/admin/api/oauth/cc/start", get(cc::start))
        .route("/admin/api/oauth/cc/callback", post(cc::callback))
        .route("/admin/api/oauth/cc/manual", post(cc::manual))
        .with_state(state)
}

mod admin;
mod api;
mod app;
mod config;
mod db;
mod engine;
mod error;
mod middleware;
mod providers;
mod services;
mod state;
mod types;
mod utils;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let fmt = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        );
    if std::env::var("RUST_LOG_FORMAT").as_deref() == Ok("json") {
        fmt.json().init();
    } else {
        fmt.init();
    }

    let _ = dotenvy::dotenv();

    let cfg = config::loader::load()?;

    // Initialize database
    let db_url = cfg.database.url.clone();
    let pool = db::init(&db_url).await?;
    tracing::info!("Database connected: {}", db_url);

    let app_state = state::AppState::new(cfg, pool).await?;

    let listener = tokio::net::TcpListener::bind(format!(
        "{}:{}",
        app_state.config.server.host, app_state.config.server.port
    ))
    .await?;

    tracing::info!(
        "NuvCode listening on {}:{}",
        app_state.config.server.host,
        app_state.config.server.port
    );

    let router = app::build(app_state);
    axum::serve(listener, router).await?;

    Ok(())
}

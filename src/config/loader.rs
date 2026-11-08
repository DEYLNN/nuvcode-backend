use crate::config::models::AppConfig;

pub fn load() -> anyhow::Result<AppConfig> {
    // Set defaults from file first, then env vars override
    let settings = config::Config::builder()
        .add_source(config::File::with_name("config/config"))
        .add_source(
            config::Environment::with_prefix("NUV")
                .separator("__")
                .try_parsing(true)
                .list_separator(","),
        )
        .build()?;

    let mut cfg: AppConfig = settings.try_deserialize()?;

    // Re-read auth env directly — config crate path mangling for env vars
    // is unreliable across versions. Read 2 underscore format (NUV_AUTH__FOO)
    // and dot-replace format (NUV_AUTH.FOO) both, env > config.
    if let Ok(v) = std::env::var("NUV_AUTH__ADMIN_PASSWORD") {
        cfg.auth.admin_password = Some(v);
    }
    if let Ok(v) = std::env::var("NUV_AUTH__JWT_SECRET") {
        cfg.auth.jwt_secret = Some(v);
    }
    if let Ok(v) = std::env::var("NUV_AUTH__ADMIN_USERNAME") {
        cfg.auth.admin_username = Some(v);
    }

    // Tracing-style sanity check (only prints when admin_password missing)
    if cfg.auth.admin_password.is_none() {
        eprintln!(
            "WARN: admin_password missing. Set NUV_AUTH__ADMIN_PASSWORD or auth.admin_password in config.toml"
        );
    }

    Ok(cfg)
}

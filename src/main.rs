mod config;
mod fastcgi;
mod php_instance;
mod php_sapi;
mod pool_manager;
mod request;
mod reset;

use anyhow::Result;

// NOT using #[tokio::main] — Tokio runtime must not exist before fork().
// Each forked worker creates its own runtime.
fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tracing::info!("rust-fpm starting");

    // Load config (CLI args + optional TOML file)
    let config = config::Config::load()?;
    tracing::info!(
        "Loaded {} pool(s), {} total workers",
        config.pools.len(),
        config.pools.iter().map(|p| p.pm_max_children).sum::<usize>()
    );

    // 1. Bind all pool sockets BEFORE fork (may need root for /var/run/)
    let mut pool_listeners = Vec::new();
    for pool in &config.pools {
        let _ = std::fs::remove_file(&pool.listen);
        let listener = std::os::unix::net::UnixListener::bind(&pool.listen)
            .map_err(|e| anyhow::anyhow!("Failed to bind socket {}: {}", pool.listen, e))?;
        pool_manager::set_socket_permissions(pool)?;
        tracing::info!("Bound socket: {}", pool.listen);
        pool_listeners.push(listener);
    }

    // 2. Init PHP SAPI with OPcache BEFORE fork — all children inherit this state
    let php_ini = config.pools.first().and_then(|p| p.php_ini.as_deref());
    let _sapi_ptr = php_sapi::init_sapi(php_ini);
    tracing::info!("PHP SAPI initialized (OPcache loaded if configured)");

    // 3. Fork workers and supervise (blocks until shutdown)
    pool_manager::run(config, pool_listeners)?;

    // 4. Cleanup (master only, after all children exited)
    php_sapi::shutdown_sapi();

    Ok(())
}

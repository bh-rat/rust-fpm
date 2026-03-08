mod config;
mod fastcgi;
mod php_instance;
mod php_sapi;
mod pool_manager;
mod request;
mod reset;

use anyhow::Result;

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
    let total_workers: usize = config.pools.iter().map(|p| p.pm_max_children).sum();
    tracing::info!(
        "Loaded {} pool(s), {} total workers",
        config.pools.len(),
        total_workers
    );

    // Bind all pool sockets
    let mut pool_listeners = Vec::new();
    for pool in &config.pools {
        let _ = std::fs::remove_file(&pool.listen);
        let listener = std::os::unix::net::UnixListener::bind(&pool.listen)
            .map_err(|e| anyhow::anyhow!("Failed to bind socket {}: {}", pool.listen, e))?;
        pool_manager::set_socket_permissions(pool)?;
        tracing::info!("Bound socket: {}", pool.listen);
        pool_listeners.push(listener);
    }

    // Init PHP SAPI + module startup (once, before any threads)
    let php_ini = config.pools.first().and_then(|p| p.php_ini.as_deref());
    let _sapi_ptr = php_sapi::init_sapi(php_ini);
    tracing::info!("PHP SAPI initialized (OPcache loaded if configured)");

    // Create Tokio runtime with N blocking threads for PHP execution
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(total_workers)
        .enable_all()
        .build()?;

    tracing::info!(
        "Tokio runtime: 2 async threads, {} blocking threads",
        total_workers
    );

    // Run accept loop (blocks until shutdown)
    rt.block_on(async {
        pool_manager::run_threaded(config, pool_listeners).await
    })?;

    // Cleanup
    php_sapi::shutdown_sapi();

    Ok(())
}

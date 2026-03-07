mod config;
mod fastcgi;
mod php_instance;
mod php_sapi;
mod pool_manager;
mod request;
mod reset;
mod worker_pool;

use std::sync::Arc;

use anyhow::Result;

// NOT using #[tokio::main] — Tokio runtime must not exist before fork().
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
        "Loaded {} pool(s), multi_process={}",
        config.pools.len(),
        config.needs_multi_process()
    );

    if config.needs_multi_process() {
        // Multi-pool mode: fork per pool, each child drops privileges.
        // No Tokio runtime yet — safe to fork.
        pool_manager::run(config)?;
    } else {
        // Single-pool mode: no fork needed, run directly.
        run_single_pool(&config)?;
    }

    Ok(())
}

/// Run a single pool directly (no fork, no privilege drop).
/// Used when no `user` field is configured — works without root.
fn run_single_pool(config: &config::Config) -> Result<()> {
    let pool_config = &config.pools[0];
    tracing::info!(
        "Single pool '{}': listen={} workers={}",
        pool_config.name,
        pool_config.listen,
        pool_config.pm_max_children
    );

    // Initialize PHP SAPI (primary/linked instance)
    let sapi_ptr = php_sapi::init_sapi(pool_config.php_ini.as_deref());
    tracing::info!("PHP SAPI initialized");

    // Create primary PhpInstance wrapping the linked libphp.so
    let primary = php_instance::PhpInstance::primary(sapi_ptr);

    // Discover libphp.so path for dlmopen'd worker instances
    let libphp_path = php_instance::find_libphp_path();

    // Create worker pool (worker 0 = primary with OPcache, workers 1+ = dlmopen'd without)
    // Note: OPcache cannot be loaded in dlmopen'd namespaces due to glibc limitations
    // (add_to_global_resize crash when loading opcache.so via dlopen inside dlmopen).
    let pool = Arc::new(worker_pool::WorkerPool::new(
        pool_config.pm_max_children,
        primary,
        libphp_path,
        pool_config.php_ini.clone(),
    ));
    tracing::info!("Worker pool created with {} workers", pool.num_workers());

    // Create Tokio runtime and run FastCGI listener
    let rt = tokio::runtime::Runtime::new()?;
    let result = rt.block_on(fastcgi::serve(&pool_config.listen, pool));

    // Cleanup
    php_sapi::shutdown_sapi();

    result
}

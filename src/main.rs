mod config;
mod fastcgi;
mod php_sapi;
mod request;
mod reset;
mod worker_pool;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tracing::info!("rust-fpm starting");

    // Parse config from CLI args
    let config = config::Config::from_args();
    tracing::info!("listen={} workers={}", config.listen, config.max_children);

    // Initialize PHP SAPI (once, before any threads)
    let _sapi_ptr = php_sapi::init_sapi(config.php_ini_path.as_deref());
    tracing::info!("PHP SAPI initialized");

    // Start FastCGI listener (runs until process exit)
    let result = fastcgi::serve(&config.listen).await;

    // Cleanup on exit
    php_sapi::shutdown_sapi();

    result
}

# rust-fpm

A drop-in replacement for php-fpm, built in Rust. Same NGINX config, same Unix socket, same WordPress — 76% less memory.

## What This Is

rust-fpm embeds the PHP interpreter (libphp.so) inside a Rust binary and serves FastCGI requests on a Unix socket, exactly like php-fpm does. NGINX doesn't know the difference. WordPress, Laravel, Drupal — any PHP application runs without modification.

The key difference is architecture. php-fpm forks a separate process for every worker, each carrying a full copy of the PHP runtime in memory. rust-fpm uses a single process with a custom SAPI (Server API) built on [ext-php-rs](https://github.com/davidcole1340/ext-php-rs), sharing OPcache across all workers. The result: you can run 4-8x more workers in the same RAM.

## How It Works

```
NGINX ──FastCGI──> Unix Socket ──> rust-fpm ──> PHP (libphp.so)
                                      │
                                      ├── FastCGI accept (tokio-fastcgi, async)
                                      ├── Dispatch to blocking thread pool
                                      ├── php_request_startup()
                                      ├── Set $_SERVER from FastCGI params
                                      ├── php_execute_script()
                                      ├── Capture output via ub_write callback
                                      ├── php_request_shutdown()
                                      └── Send response back through FastCGI
```

rust-fpm implements PHP's SAPI interface — the same callback layer that php-fpm, Apache mod_php, and the CLI all implement. The custom SAPI handles output capture (`ub_write`), POST body reading (`read_post`), header collection (`send_header`), `$_SERVER` population (`register_server_variables`), and fatal error recovery via `setjmp`/`longjmp`.

Two execution models are available:

- **NTS fork model** (`main` branch): Master process initializes PHP and OPcache, then forks N workers. Each worker inherits the shared OPcache SHM and runs a Tokio runtime with one blocking thread. Same isolation guarantees as php-fpm.
- **ZTS thread model** (`zts-threads` branch): Single process, N blocking threads with per-thread PHP globals via TSRM. Requires ZTS-compiled PHP. Lower memory footprint, higher effective concurrency.

## Benchmark Results

Tested on WordPress 6.x homepage, PHP 8.3.15, MariaDB 10.11, Docker on Apple Silicon. OPcache enabled (128MB, JIT disabled) for all configurations.

### ZTS Thread Model vs php-fpm (ZTS)

**WordPress homepage, wrk -t2 -d30s:**

| Workers | php-fpm req/s | php-fpm RSS | rust-fpm req/s | rust-fpm RSS | Memory Savings |
|---------|--------------|-------------|---------------|-------------|----------------|
| 4       | 139          | 181 MB      | 134           | 105 MB      | 42%            |
| 8       | 191          | 349 MB      | 162           | 143 MB      | 59%            |
| 16      | 189          | 683 MB      | 192           | 203 MB      | 70%            |
| 32      | 159          | 1352 MB     | 163           | 319 MB      | **76%**        |

**I/O-heavy workloads (10 MySQL queries per request):**

| Workers | php-fpm req/s | rust-fpm req/s |
|---------|--------------|---------------|
| 4       | 1,753        | 1,548         |
| 32      | 1,162        | **2,520**     |

At 32 workers with database-heavy workloads, rust-fpm delivers 2.2x the throughput of php-fpm.

**The value proposition**: at equal memory budget, rust-fpm runs more workers and serves more requests. php-fpm needs 1.3 GB for 32 workers; rust-fpm needs 319 MB.

## Installation

rust-fpm requires Linux with PHP compiled using `--enable-embed=shared`. macOS does not ship the embed SAPI — use the provided Docker environment for development.

### Docker (recommended)

```bash
git clone https://github.com/bh-rat/rust-fpm.git
cd rust-fpm

# Start development container with PHP embed + MariaDB
cd test-env
docker compose up -d
docker compose exec dev bash

# Inside the container:
cd /rust-fpm
cargo build --release
./setup-wordpress.sh

# Run rust-fpm
./target/release/rust-fpm --listen /var/run/php-fpm.sock --workers 4
```

WordPress is now served via NGINX on port 8080.

### Configuration

rust-fpm accepts php-fpm-compatible TOML configuration:

```bash
# CLI usage
rust-fpm --listen /var/run/php-fpm.sock --workers 4

# Or with a config file
rust-fpm --config /etc/rust-fpm/www.conf
```

NGINX configuration is identical to php-fpm — just point `fastcgi_pass` at the socket:

```nginx
location ~ \.php$ {
    fastcgi_pass unix:/var/run/php-fpm.sock;
    include fastcgi_params;
    fastcgi_param SCRIPT_FILENAME $document_root$fastcgi_script_name;
}
```

## Project Structure

```
src/
├── main.rs           # Entry point, runtime setup, jemalloc
├── config.rs         # TOML config parsing (php-fpm compatible keys)
├── fastcgi.rs        # FastCGI accept loop, response formatting
├── php_sapi.rs       # Custom SAPI callbacks (ub_write, read_post, etc.)
├── php_instance.rs   # PHP request lifecycle, fatal error recovery
├── pool_manager.rs   # Worker management (fork or thread model)
├── request.rs        # FastCGI params → $_SERVER mapping
└── reset.rs          # Per-request state cleanup
```

## Testing

The `test-env/test-scripts/` directory covers WordPress edge cases:

- POST body handling and file uploads
- Session persistence across requests
- Cookie round-trips
- Custom header emission (`header()` after output)
- Error handling and fatal error recovery
- Per-request state isolation
- MySQL query workloads (configurable query count)
- OPcache status verification

## Future Work

- [ ] Benchmark against FrankenPHP (classic mode) on WordPress
- [ ] Apply performance optimizations to NTS fork model
- [ ] `php://input` stream support for raw POST body access
- [ ] File upload handling with temp file management
- [ ] `getallheaders()` implementation in SAPI
- [ ] Configurable request timeouts (`request_terminate_timeout`)
- [ ] Graceful reload (finish in-flight requests on SIGHUP)
- [ ] Connection pooling for persistent MySQL connections
- [ ] ARM64 and x86_64 prebuilt binaries

## License

MIT License. See [LICENSE](LICENSE) for details.

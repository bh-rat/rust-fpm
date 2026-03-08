# rust-fpm

An experimental drop-in replacement for php-fpm, built in Rust. Uses the same NGINX configuration and Unix socket interface.

## What This Is

rust-fpm embeds the PHP interpreter (libphp.so) inside a Rust binary and serves FastCGI requests on a Unix socket, the same way php-fpm does. It implements PHP's SAPI interface — the callback layer that php-fpm, Apache mod_php, and the CLI all use — so existing PHP applications work without modification.

The project explores whether a Rust-based process manager can reduce per-worker memory overhead compared to php-fpm's fork-per-worker model. It is not production-ready.

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

The custom SAPI handles output capture (`ub_write`), POST body reading (`read_post`), header collection (`send_header`), `$_SERVER` population (`register_server_variables`), and fatal error recovery via `setjmp`/`longjmp`. It uses [ext-php-rs](https://github.com/davidcole1340/ext-php-rs) for PHP bindings and [tokio-fastcgi](https://crates.io/crates/tokio-fastcgi) for the FastCGI protocol.

Two execution models exist:

- **NTS fork model** (`main` branch): Master process initializes PHP and OPcache, then forks N workers. Each worker inherits shared OPcache SHM and runs a Tokio runtime with one blocking thread.
- **ZTS thread model** (`zts-threads` branch): Single process, N blocking threads with per-thread PHP globals via TSRM. Requires ZTS-compiled PHP.

## Benchmark Results

All benchmarks run in Docker on Apple Silicon (not bare metal). Numbers have 10-20% run-to-run variance in this environment. Take them as directional, not absolute.

Tested with WordPress 6.x homepage, PHP 8.3.15, MariaDB 10.11. OPcache enabled (128MB, JIT disabled) for both php-fpm and rust-fpm.

### ZTS Thread Model vs php-fpm (ZTS)

**WordPress homepage (wrk -t2 -d30s):**

| Workers | php-fpm req/s | php-fpm RSS | rust-fpm req/s | rust-fpm RSS |
|---------|--------------|-------------|---------------|-------------|
| 4       | 139          | 181 MB      | 134           | 105 MB      |
| 8       | 191          | 349 MB      | 162           | 143 MB      |
| 16      | 189          | 683 MB      | ~120-192*     | 203 MB      |
| 32      | 159          | 1352 MB     | ~138-163*     | 319 MB      |

*High variance at 16+ workers in Docker. Needs bare-metal validation.

**10 MySQL queries per request:**

| Workers | php-fpm req/s | rust-fpm req/s |
|---------|--------------|---------------|
| 4       | 1,753        | 1,548         |
| 32      | 1,162        | 2,520         |

### What the data shows

- **Throughput**: rust-fpm is roughly 5-15% slower than php-fpm on WordPress at low worker counts. At higher worker counts and I/O-heavy workloads, the gap narrows or reverses. The WordPress throughput gap is partly explained by response buffering overhead (rust-fpm buffers the full response before sending; php-fpm streams directly to the socket).
- **Memory**: rust-fpm uses less RSS than php-fpm at every worker count tested. php-fpm RSS scales roughly linearly with worker count; rust-fpm scales sub-linearly because threads share the process address space.
- **Not tested**: production traffic patterns, long-running requests, file uploads, high-concurrency edge cases, bare-metal performance.

## Installation

Requires Linux with PHP compiled using `--enable-embed=shared`. macOS does not ship the embed SAPI — use the provided Docker environment.

### Docker

```bash
git clone https://github.com/bh-rat/rust-fpm.git
cd rust-fpm

# Start dev container with PHP embed + MariaDB
cd test-env
docker compose up -d
docker compose exec dev bash

# Inside container:
cd /rust-fpm
cargo build --release
./setup-wordpress.sh
./target/release/rust-fpm --listen /var/run/php-fpm.sock --workers 4
```

WordPress is served via NGINX on port 8080.

### Configuration

```bash
# CLI
rust-fpm --listen /var/run/php-fpm.sock --workers 4

# Config file
rust-fpm --config /etc/rust-fpm/www.conf
```

NGINX config is identical to php-fpm:

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
├── main.rs           # Entry point, runtime setup, jemalloc allocator
├── config.rs         # TOML config parsing (php-fpm compatible keys)
├── fastcgi.rs        # FastCGI accept loop, response formatting
├── php_sapi.rs       # Custom SAPI callbacks (ub_write, read_post, etc.)
├── php_instance.rs   # PHP request lifecycle, fatal error recovery
├── pool_manager.rs   # Worker management (fork or thread model)
├── request.rs        # FastCGI params → $_SERVER mapping
└── reset.rs          # Per-request state cleanup
```

## Known Limitations

- No `php://input` stream support (raw POST body not accessible)
- No file upload handling
- No `getallheaders()` implementation
- No request timeout enforcement
- No graceful reload (SIGHUP)
- Benchmarks are Docker-only; bare-metal numbers needed
- Not tested with PHP extensions beyond core + OPcache + mysqli

## Future Work

- [ ] Bare-metal benchmarks on Linux
- [ ] Benchmark against FrankenPHP (classic mode)
- [ ] Apply optimizations to NTS fork model
- [ ] File upload and `php://input` support
- [ ] Request timeouts
- [ ] Graceful reload

## License

MIT License. See [LICENSE](LICENSE) for details.

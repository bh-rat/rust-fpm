use std::sync::Arc;
use tokio::net::UnixListener;
use tokio_fastcgi::{Requests, RequestResult};
use anyhow::Result;

use crate::php_sapi::RequestContext;
use crate::request;
use crate::worker_pool::WorkerPool;

/// Start the FastCGI listener on a new socket and process requests via the worker pool.
pub async fn serve(listen_path: &str, pool: Arc<WorkerPool>) -> Result<()> {
    // Remove stale socket file if present
    let _ = std::fs::remove_file(listen_path);

    let listener = UnixListener::bind(listen_path)?;
    tracing::info!("Listening on {}", listen_path);

    // Set socket permissions so NGINX (www-data) can connect
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(listen_path, std::fs::Permissions::from_mode(0o666))?;
    }

    accept_loop(listener, pool).await
}

/// Start the FastCGI listener on a pre-bound socket (used by pool_manager after privilege drop).
pub async fn serve_on_listener(
    listener: std::os::unix::net::UnixListener,
    pool: Arc<WorkerPool>,
) -> Result<()> {
    listener.set_nonblocking(true)?;
    let listener = UnixListener::from_std(listener)?;
    accept_loop(listener, pool).await
}

/// Core accept loop shared by both entry points.
async fn accept_loop(listener: UnixListener, pool: Arc<WorkerPool>) -> Result<()> {
    loop {
        let (stream, _addr) = listener.accept().await?;
        tracing::debug!("New FastCGI connection");

        let pool = pool.clone();
        tokio::spawn(async move {
            let mut requests = Requests::from_split_socket(stream.into_split(), 10, 10);

            while let Ok(Some(request)) = requests.next().await {
                let pool = pool.clone();
                if let Err(e) = request
                    .process(|req| async move { process_request(req, pool).await })
                    .await
                {
                    tracing::error!("FastCGI request error: {:?}", e);
                }
            }
        });
    }
}

async fn process_request<W: tokio::io::AsyncWrite + Unpin + Send + 'static>(
    req: Arc<tokio_fastcgi::Request<W>>,
    pool: Arc<WorkerPool>,
) -> RequestResult {
    // Extract params and POST body (sync — data already buffered)
    let params = request::extract_params(&req);
    let post_body = request::read_stdin(&req);

    let method = params
        .get("REQUEST_METHOD")
        .cloned()
        .unwrap_or_else(|| "GET".into());
    let uri = params
        .get("REQUEST_URI")
        .cloned()
        .unwrap_or_else(|| "/".into());
    let script = params
        .get("SCRIPT_FILENAME")
        .cloned()
        .unwrap_or_default();

    tracing::info!("{} {} -> {}", method, uri, script);

    if script.is_empty() {
        let _ = req
            .get_stdout()
            .write(b"Status: 404 Not Found\r\nContent-Type: text/plain\r\n\r\nNo SCRIPT_FILENAME")
            .await;
        return RequestResult::Complete(0);
    }

    // Execute PHP via worker pool
    let ctx = RequestContext::new(params, post_body);
    let result = pool.execute(ctx).await;

    match result {
        Ok(ctx) => {
            let response = format_response(&ctx);
            let _ = req.get_stdout().write(&response).await;
            RequestResult::Complete(0)
        }
        Err(e) => {
            tracing::error!("Pool execute error: {:?}", e);
            let _ = req
                .get_stdout()
                .write(b"Status: 500 Internal Server Error\r\nContent-Type: text/plain\r\n\r\nInternal error")
                .await;
            RequestResult::Complete(1)
        }
    }
}

/// Format the HTTP response for FastCGI STDOUT.
/// Format: Status: NNN\r\nHeader: Value\r\n\r\nBody
fn format_response(ctx: &RequestContext) -> Vec<u8> {
    let mut response = Vec::with_capacity(ctx.output_buffer.len() + 1024);

    // Status line
    let status_text = match ctx.http_status_code {
        200 => "OK",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "OK",
    };
    response.extend_from_slice(
        format!("Status: {} {}\r\n", ctx.http_status_code, status_text).as_bytes(),
    );

    // Response headers from PHP
    let mut has_content_type = false;
    for header in &ctx.response_headers {
        response.extend_from_slice(header.as_bytes());
        response.extend_from_slice(b"\r\n");
        if header.len() > 12
            && header.as_bytes()[..12].eq_ignore_ascii_case(b"content-type")
        {
            has_content_type = true;
        }
    }

    // Default Content-Type if PHP didn't set one
    if !has_content_type {
        response.extend_from_slice(b"Content-Type: text/html; charset=UTF-8\r\n");
    }

    // Blank line separating headers from body
    response.extend_from_slice(b"\r\n");

    // Body
    response.extend_from_slice(&ctx.output_buffer);

    response
}

use std::sync::Arc;
use tokio::net::UnixListener;
use tokio_fastcgi::{Requests, RequestResult};
use anyhow::Result;

use crate::php_sapi::RequestContext;
use crate::request;

/// Accept loop for thread-based model. Spawns each connection concurrently.
/// PHP executes via spawn_blocking — Tokio's blocking thread pool provides concurrency.
pub async fn serve(listener: std::os::unix::net::UnixListener) -> Result<()> {
    listener.set_nonblocking(true)?;
    let listener = UnixListener::from_std(listener)?;
    tracing::info!("Accepting FastCGI connections");

    loop {
        let (stream, _) = listener.accept().await?;
        tracing::debug!("New FastCGI connection");

        // Spawn each connection concurrently — multiple PHP requests in flight
        tokio::spawn(async move {
            let mut requests = Requests::from_split_socket(stream.into_split(), 10, 10);
            while let Ok(Some(request)) = requests.next().await {
                if let Err(e) = request
                    .process(|req| async move { process_request(req).await })
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

    // Execute PHP via spawn_blocking (blocks one thread, Tokio handles IO)
    let ctx = RequestContext::new(params, post_body);

    let result = tokio::task::spawn_blocking(move || {
        crate::php_instance::execute_request(ctx)
    })
    .await;

    match result {
        Ok(ctx) => {
            // Write headers and body separately — avoids copying the entire body
            let headers = format_headers(&ctx);
            let _ = req.get_stdout().write(&headers).await;
            let _ = req.get_stdout().write(&ctx.output_buffer).await;

            RequestResult::Complete(0)
        }
        Err(e) => {
            tracing::error!("PHP execution panic: {:?}", e);
            let _ = req
                .get_stdout()
                .write(b"Status: 500 Internal Server Error\r\nContent-Type: text/plain\r\n\r\nInternal error")
                .await;
            RequestResult::Complete(1)
        }
    }
}

/// Format just the HTTP headers for FastCGI STDOUT (no body copy).
/// Format: Status: NNN Reason\r\nHeader: Value\r\n...\r\n
fn format_headers(ctx: &RequestContext) -> Vec<u8> {
    let mut headers = Vec::with_capacity(1024);

    // Status line — write directly without format! allocation
    headers.extend_from_slice(b"Status: ");
    let mut buf = itoa::Buffer::new();
    headers.extend_from_slice(buf.format(ctx.http_status_code).as_bytes());
    headers.extend_from_slice(b" ");
    headers.extend_from_slice(match ctx.http_status_code {
        200 => b"OK" as &[u8],
        301 => b"Moved Permanently",
        302 => b"Found",
        304 => b"Not Modified",
        400 => b"Bad Request",
        401 => b"Unauthorized",
        403 => b"Forbidden",
        404 => b"Not Found",
        500 => b"Internal Server Error",
        502 => b"Bad Gateway",
        503 => b"Service Unavailable",
        _ => b"OK",
    });
    headers.extend_from_slice(b"\r\n");

    // Response headers from PHP
    let mut has_content_type = false;
    for header in &ctx.response_headers {
        headers.extend_from_slice(header);
        headers.extend_from_slice(b"\r\n");
        if header.len() > 12
            && header[..12].eq_ignore_ascii_case(b"content-type")
        {
            has_content_type = true;
        }
    }

    // Default Content-Type if PHP didn't set one
    if !has_content_type {
        headers.extend_from_slice(b"Content-Type: text/html; charset=UTF-8\r\n");
    }

    // Blank line separating headers from body
    headers.extend_from_slice(b"\r\n");

    headers
}

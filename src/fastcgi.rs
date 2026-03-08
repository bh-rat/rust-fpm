use std::sync::Arc;
use std::time::Instant;
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
    let t0 = Instant::now();

    // Extract params and POST body (sync — data already buffered)
    let params = request::extract_params(&req);
    let post_body = request::read_stdin(&req);

    let t1 = Instant::now(); // T1: params extracted

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
    let t2 = Instant::now(); // T2: about to enter spawn_blocking

    let result = tokio::task::spawn_blocking(move || {
        let t3 = Instant::now(); // T3: inside blocking thread
        let ctx = crate::php_instance::execute_request(ctx);
        let t4 = Instant::now(); // T4: PHP done
        (ctx, t3, t4)
    })
    .await;

    let t5 = Instant::now(); // T5: spawn_blocking returned

    match result {
        Ok((ctx, t3, t4)) => {
            let response = format_response(&ctx);
            let t6 = Instant::now(); // T6: response formatted
            let _ = req.get_stdout().write(&response).await;
            let t7 = Instant::now(); // T7: response written

            // Log timing breakdown
            tracing::info!(
                "TIMING: total={:.2}ms | params={:.2}ms | handoff={:.2}ms | php={:.2}ms | return={:.2}ms | format={:.2}ms | write={:.2}ms | output={}B",
                t7.duration_since(t0).as_secs_f64() * 1000.0,
                t1.duration_since(t0).as_secs_f64() * 1000.0,
                t3.duration_since(t2).as_secs_f64() * 1000.0,
                t4.duration_since(t3).as_secs_f64() * 1000.0,
                t5.duration_since(t4).as_secs_f64() * 1000.0,
                t6.duration_since(t5).as_secs_f64() * 1000.0,
                t7.duration_since(t6).as_secs_f64() * 1000.0,
                response.len(),
            );

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

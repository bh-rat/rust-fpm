use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;
use tokio::io::AsyncWrite;
use tokio_fastcgi::Request;

/// Extract all FastCGI params from request, converting to uppercase keys.
/// tokio-fastcgi lowercases all keys; PHP expects uppercase for $_SERVER.
pub fn extract_params<W: AsyncWrite + Unpin + Send>(
    request: &Arc<Request<W>>,
) -> HashMap<String, String> {
    let mut params = HashMap::new();

    if let Some(iter) = request.str_params_iter() {
        for (key, value) in iter {
            let upper_key = key.to_ascii_uppercase();
            params.insert(upper_key, value.unwrap_or("").to_string());
        }
    }

    // Ensure critical defaults
    if !params.contains_key("REQUEST_METHOD") {
        params.insert("REQUEST_METHOD".into(), "GET".into());
    }
    if !params.contains_key("SERVER_PROTOCOL") {
        params.insert("SERVER_PROTOCOL".into(), "HTTP/1.1".into());
    }
    if !params.contains_key("GATEWAY_INTERFACE") {
        params.insert("GATEWAY_INTERFACE".into(), "CGI/1.1".into());
    }

    // REQUEST_TIME and REQUEST_TIME_FLOAT
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    params
        .entry("REQUEST_TIME".into())
        .or_insert_with(|| now.as_secs().to_string());
    params
        .entry("REQUEST_TIME_FLOAT".into())
        .or_insert_with(|| format!("{:.6}", now.as_secs_f64()));

    params
}

/// Read full STDIN (POST body) from the FastCGI request.
/// tokio-fastcgi buffers all STDIN before the callback runs, so this is sync.
pub fn read_stdin<W: AsyncWrite + Unpin + Send>(request: &Arc<Request<W>>) -> Vec<u8> {
    let mut body = Vec::new();
    let mut stdin = request.get_stdin();
    let _ = stdin.read_to_end(&mut body);
    body
}

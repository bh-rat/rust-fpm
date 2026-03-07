use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CStr, CString};

use ext_php_rs::builders::SapiBuilder;
use ext_php_rs::embed::SapiModule;
use ext_php_rs::ffi::{
    sapi_header_struct, sapi_headers_struct, sapi_module_struct, sapi_startup,
};
use ext_php_rs::types::Zval;

// php_register_variable is exported from libphp.so but not re-exported by ext-php-rs
unsafe extern "C" {
    fn php_register_variable(
        var: *const c_char,
        val: *const c_char,
        track_vars_array: *mut Zval,
    );
    fn php_module_startup(sf: *mut sapi_module_struct, additional_module: *mut c_void) -> c_int;
    fn php_module_shutdown();
}

/// Get raw sapi_globals pointer for this process's PHP instance.
/// After fork, each process has exactly one set of PHP globals.
#[inline]
unsafe fn raw_sapi_globals() -> *mut ext_php_rs::ffi::sapi_globals_struct {
    unsafe { ext_php_rs::ffi::ext_php_rs_sapi_globals() }
}

// ---------------------------------------------------------------------------
// RequestContext — per-request state passed through SapiGlobals::server_context
// ---------------------------------------------------------------------------

pub struct RequestContext {
    /// FastCGI PARAMS with uppercase keys
    pub params: HashMap<String, String>,
    /// POST body from FastCGI STDIN
    pub post_body: Vec<u8>,
    /// How many POST bytes have been read by PHP so far
    pub post_read_offset: usize,
    /// Captured PHP output
    pub output_buffer: Vec<u8>,
    /// Captured response headers ("Name: Value" strings)
    pub response_headers: Vec<String>,
    /// HTTP status code from PHP
    pub http_status_code: u16,
    /// Set by deactivate when request lifecycle ends
    pub request_finished: bool,
}

impl RequestContext {
    pub fn new(params: HashMap<String, String>, post_body: Vec<u8>) -> Self {
        Self {
            params,
            post_body,
            post_read_offset: 0,
            output_buffer: Vec::with_capacity(65536),
            response_headers: Vec::new(),
            http_status_code: 200,
            request_finished: false,
        }
    }

    /// Transfer ownership to a raw pointer for server_context.
    pub fn into_raw(self) -> *mut c_void {
        Box::into_raw(Box::new(self)).cast()
    }

    /// Borrow mutably from server_context pointer (does NOT take ownership).
    ///
    /// # Safety
    /// Caller must ensure `ptr` is a valid pointer to a `RequestContext` that
    /// is not aliased.
    pub unsafe fn from_server_context<'a>(ptr: *mut c_void) -> &'a mut Self {
        unsafe { &mut *ptr.cast::<Self>() }
    }

    /// Take ownership back from a raw pointer.
    ///
    /// # Safety
    /// Caller must ensure `ptr` was created by `into_raw` and has not been
    /// reclaimed yet.
    pub unsafe fn from_raw(ptr: *mut c_void) -> Box<Self> {
        unsafe { Box::from_raw(ptr.cast::<Self>()) }
    }

    /// Create an error response without executing PHP.
    /// Used by worker pool for panic recovery and channel errors.
    pub fn error_response(status: u16, message: &str) -> Self {
        Self {
            params: HashMap::new(),
            post_body: Vec::new(),
            post_read_offset: 0,
            output_buffer: message.as_bytes().to_vec(),
            response_headers: Vec::new(),
            http_status_code: status,
            request_finished: true,
        }
    }
}

// ---------------------------------------------------------------------------
// SAPI callbacks — all extern "C" fn
// Use raw_sapi_globals() instead of SapiGlobals::get() to avoid RwLock issues
// when called from PHP's C code.
// ---------------------------------------------------------------------------

/// Called by PHP when it writes output (echo, print, etc.)
extern "C" fn sapi_ub_write(str: *const c_char, str_length: usize) -> usize {
    if str.is_null() || str_length == 0 {
        return 0;
    }

    let server_ctx = unsafe { (*raw_sapi_globals()).server_context };
    if server_ctx.is_null() {
        return str_length;
    }

    let ctx = unsafe { RequestContext::from_server_context(server_ctx) };
    if ctx.request_finished {
        return 0;
    }

    let slice = unsafe { std::slice::from_raw_parts(str.cast::<u8>(), str_length) };
    ctx.output_buffer.extend_from_slice(slice);
    str_length
}

/// Called by PHP to flush output — no-op, we buffer everything.
extern "C" fn sapi_flush(_server_context: *mut c_void) {}

/// Called by PHP for each response header.
extern "C" fn sapi_send_header(header: *mut sapi_header_struct, server_context: *mut c_void) {
    if server_context.is_null() || header.is_null() {
        return;
    }
    let sapi_header = unsafe { &*header };
    if sapi_header.header.is_null() || sapi_header.header_len == 0 {
        return;
    }
    let header_bytes = unsafe {
        std::slice::from_raw_parts(sapi_header.header.cast::<u8>(), sapi_header.header_len)
    };
    if let Ok(header_str) = std::str::from_utf8(header_bytes) {
        let ctx = unsafe { RequestContext::from_server_context(server_context) };
        ctx.response_headers.push(header_str.to_string());
    }
}

/// Called by PHP when all headers are finalized — capture status code.
/// Returns SAPI_HEADER_DO_SEND (2) so PHP iterates headers and calls send_header for each.
/// CRITICAL: Must NOT return 0 — that causes PHP to disable all output.
extern "C" fn sapi_send_headers(sapi_headers: *mut sapi_headers_struct) -> c_int {
    if !sapi_headers.is_null() {
        let headers = unsafe { &*sapi_headers };
        let server_ctx = unsafe { (*raw_sapi_globals()).server_context };
        if !server_ctx.is_null() {
            let ctx = unsafe { RequestContext::from_server_context(server_ctx) };
            let code = headers.http_response_code as u16;
            ctx.http_status_code = if code == 0 { 200 } else { code };
        }
    }
    2 // SAPI_HEADER_DO_SEND — tell PHP to iterate and call send_header for each
}

/// Called by PHP to read POST body.
extern "C" fn sapi_read_post(buffer: *mut c_char, length: usize) -> usize {
    let sg = unsafe { &*raw_sapi_globals() };
    if sg.server_context.is_null() {
        return 0;
    }
    let content_length = sg.request_info.content_length;
    if content_length <= 0 {
        return 0;
    }
    if sg.read_post_bytes as usize >= content_length as usize {
        return 0;
    }

    let ctx = unsafe { RequestContext::from_server_context(sg.server_context) };
    let remaining = ctx.post_body.len().saturating_sub(ctx.post_read_offset);
    let to_read = length.min(remaining);
    if to_read == 0 {
        return 0;
    }

    unsafe {
        std::ptr::copy_nonoverlapping(
            ctx.post_body[ctx.post_read_offset..].as_ptr(),
            buffer.cast::<u8>(),
            to_read,
        );
    }
    ctx.post_read_offset += to_read;
    to_read
}

/// Called by PHP to read cookies.
/// Returns a raw pointer that PHP reads but does NOT free.
/// We free it in deactivate via cookie_data.
extern "C" fn sapi_read_cookies() -> *mut c_char {
    let server_ctx = unsafe { (*raw_sapi_globals()).server_context };
    if server_ctx.is_null() {
        return std::ptr::null_mut();
    }
    let ctx = unsafe { RequestContext::from_server_context(server_ctx) };
    ctx.params
        .get("HTTP_COOKIE")
        .and_then(|v| CString::new(v.as_str()).ok())
        .map(|c| c.into_raw()) // ownership transferred; freed in deactivate
        .unwrap_or(std::ptr::null_mut())
}

/// Called by PHP to populate $_SERVER.
/// Dispatches php_register_variable through TLS to support dlmopen'd instances.
extern "C" fn sapi_register_server_variables(track_vars_array: *mut Zval) {
    let server_ctx = unsafe { (*raw_sapi_globals()).server_context };
    if server_ctx.is_null() {
        return;
    }
    let ctx = unsafe { RequestContext::from_server_context(server_ctx) };

    for (key, value) in &ctx.params {
        if let (Ok(c_key), Ok(c_val)) = (CString::new(key.as_str()), CString::new(value.as_str()))
        {
            unsafe {
                php_register_variable(c_key.as_ptr(), c_val.as_ptr(), track_vars_array);
            }
        }
    }

    // Always register SERVER_SOFTWARE
    if let (Ok(name), Ok(val)) = (
        CString::new("SERVER_SOFTWARE"),
        CString::new("rust-fpm/0.1.0"),
    ) {
        unsafe {
            php_register_variable(name.as_ptr(), val.as_ptr(), track_vars_array);
        }
    }
}

/// Called by PHP for error/log messages.
extern "C" fn sapi_log_message(message: *const c_char, _syslog_type: c_int) {
    if message.is_null() {
        return;
    }
    let msg = unsafe { CStr::from_ptr(message) }.to_string_lossy();
    tracing::error!("PHP: {}", msg);
}

/// Per-request activation — no-op.
extern "C" fn sapi_activate() -> c_int {
    0
}

/// Per-request deactivation — free CStrings we allocated, mark finished.
/// Does NOT take ownership of the RequestContext — execute_request does that.
extern "C" fn sapi_deactivate() -> c_int {
    let sg = unsafe { &mut *raw_sapi_globals() };
    if !sg.sapi_started || sg.server_context.is_null() {
        return 0;
    }

    // Mark context as finished
    let ctx = unsafe { RequestContext::from_server_context(sg.server_context) };
    ctx.request_finished = true;

    // Free CStrings we set in init_request_info.
    let ri = sg.request_info;
    unsafe {
        if !ri.request_method.is_null() {
            drop(CString::from_raw(ri.request_method as *mut c_char));
        }
        if !ri.query_string.is_null() {
            drop(CString::from_raw(ri.query_string));
        }
        if !ri.request_uri.is_null() {
            drop(CString::from_raw(ri.request_uri));
        }
        if !ri.content_type.is_null() {
            drop(CString::from_raw(ri.content_type as *mut c_char));
        }
        // cookie_data was allocated in read_cookies via CString::into_raw
        if !ri.cookie_data.is_null() {
            drop(CString::from_raw(ri.cookie_data));
        }
    }

    // Null out server_context
    sg.server_context = std::ptr::null_mut();

    0
}

/// Module startup callback — called once at process start.
extern "C" fn sapi_startup_cb(sapi: *mut SapiModule) -> c_int {
    unsafe { php_module_startup(sapi.cast(), std::ptr::null_mut()) }
}

/// Module shutdown callback — called once at process exit.
extern "C" fn sapi_shutdown_cb(_sapi: *mut SapiModule) -> c_int {
    unsafe { php_module_shutdown() };
    0
}

// ---------------------------------------------------------------------------
// Lifecycle functions — called from main.rs / fastcgi.rs
// ---------------------------------------------------------------------------

/// Build a sapi_module_struct with all SAPI callbacks configured.
/// Does NOT call any PHP lifecycle functions — just fills in the struct.
pub fn build_sapi_module(php_ini_path: Option<&str>) -> *mut SapiModule {
    let mut builder = SapiBuilder::new("cgi-fcgi", "Rust FPM")
        .startup_function(sapi_startup_cb)
        .shutdown_function(sapi_shutdown_cb)
        .activate_function(sapi_activate)
        .deactivate_function(sapi_deactivate)
        .ub_write_function(sapi_ub_write)
        .flush_function(sapi_flush)
        .send_header_function(sapi_send_header)
        .send_headers_function(sapi_send_headers)
        .read_post_function(sapi_read_post)
        .read_cookies_function(sapi_read_cookies)
        .register_server_variables_function(sapi_register_server_variables)
        .log_message_function(sapi_log_message);

    if let Some(ini_path) = php_ini_path {
        builder = builder.php_ini_path_override(ini_path);
    }

    builder = builder.ini_entries(
        "html_errors=0\nimplicit_flush=0\noutput_buffering=0\nzend.max_allowed_stack_size=-1\n",
    );

    let sapi_module = builder.build().expect("Failed to build SAPI module");
    sapi_module.into_raw()
}

/// Initialize the PHP SAPI module (primary/linked instance). Call once at process start.
/// Returns a raw pointer to the SapiModule (needed for PhpInstance::primary()).
pub fn init_sapi(php_ini_path: Option<&str>) -> *mut SapiModule {
    let sapi_ptr = build_sapi_module(php_ini_path);

    // Lifecycle sequence (from ext-php-rs/tests/sapi.rs):
    // 1. ext_php_rs_sapi_startup — signal setup, TSRM init
    unsafe { ext_php_rs::embed::ext_php_rs_sapi_startup() };
    // 2. sapi_startup — register module with PHP
    unsafe { sapi_startup(sapi_ptr) };
    // 3. php_module_startup — via our startup callback
    unsafe {
        if let Some(startup) = (*sapi_ptr).startup {
            startup(sapi_ptr);
        }
    }

    sapi_ptr
}

/// Shutdown the PHP SAPI. Call once at process exit.
pub fn shutdown_sapi() {
    unsafe {
        ext_php_rs::ffi::sapi_shutdown();
        ext_php_rs::embed::ext_php_rs_sapi_shutdown();
    }
}

/// Set sapi_globals.request_info fields before php_request_startup.
/// Public for use by PhpInstance::execute_request().
pub fn init_request_info(ctx: &RequestContext) {
    let sg = unsafe { &mut *raw_sapi_globals() };

    // Default status
    sg.sapi_headers.http_response_code = 200;

    // request_method (*const c_char)
    let method = ctx
        .params
        .get("REQUEST_METHOD")
        .map(|s| s.as_str())
        .unwrap_or("GET");
    if let Ok(cs) = CString::new(method) {
        sg.request_info.request_method = cs.into_raw();
    }

    // query_string (*mut c_char)
    let qs = ctx
        .params
        .get("QUERY_STRING")
        .map(|s| s.as_str())
        .unwrap_or("");
    if let Ok(cs) = CString::new(qs) {
        sg.request_info.query_string = cs.into_raw();
    }

    // request_uri (*mut c_char)
    let uri = ctx
        .params
        .get("REQUEST_URI")
        .map(|s| s.as_str())
        .unwrap_or("/");
    if let Ok(cs) = CString::new(uri) {
        sg.request_info.request_uri = cs.into_raw();
    }

    // content_type (*const c_char)
    if let Some(ct) = ctx.params.get("CONTENT_TYPE") {
        if let Ok(cs) = CString::new(ct.as_str()) {
            sg.request_info.content_type = cs.into_raw();
        }
    } else {
        sg.request_info.content_type = std::ptr::null();
    }

    // content_length (i64)
    sg.request_info.content_length = ctx
        .params
        .get("CONTENT_LENGTH")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);

    // path_translated (*mut c_char) = SCRIPT_FILENAME
    let script = ctx
        .params
        .get("SCRIPT_FILENAME")
        .map(|s| s.as_str())
        .unwrap_or("");
    if let Ok(cs) = CString::new(script) {
        sg.request_info.path_translated = cs.into_raw();
    }

    // proto_num (HTTP/1.1 = 1001)
    sg.request_info.proto_num = 1001;
}


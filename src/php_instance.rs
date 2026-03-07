use std::ffi::{c_char, c_int, c_void, CStr, CString};

use ext_php_rs::ffi::{__jmp_buf_tag, zend_file_handle};
use tracing::error;

use crate::php_sapi::{self, RequestContext};

// ---------------------------------------------------------------------------
// FFI — setjmp (C standard) and PHP lifecycle functions (linked libphp.so)
// ---------------------------------------------------------------------------

unsafe extern "C" {
    // C's setjmp — takes jmp_buf which decays to __jmp_buf_tag* in function call.
    fn setjmp(env: *mut __jmp_buf_tag) -> c_int;

    fn php_request_startup() -> c_int;
    fn php_request_shutdown(dummy: *mut c_void);
    fn php_execute_script(primary_file: *mut zend_file_handle) -> bool;
    fn zend_stream_init_filename(handle: *mut zend_file_handle, filename: *const c_char);
    fn zend_destroy_file_handle(handle: *mut zend_file_handle);
}

// ---------------------------------------------------------------------------
// execute_request — run a single PHP request in this process
// ---------------------------------------------------------------------------

/// Execute a PHP request using this process's inherited PHP instance.
/// Each forked worker process has exactly one PHP context (inherited from master).
pub fn execute_request(ctx: RequestContext) -> RequestContext {
    let sapi_globals = unsafe { ext_php_rs::ffi::ext_php_rs_sapi_globals() };

    // 1. Set request_info fields
    php_sapi::init_request_info(&ctx);

    // 2. Store context in server_context
    let ctx_ptr = ctx.into_raw();
    unsafe { (*sapi_globals).server_context = ctx_ptr };

    // 3. php_request_startup
    let startup_result = unsafe { php_request_startup() };
    if startup_result != ext_php_rs::ffi::ZEND_RESULT_CODE_SUCCESS {
        error!("php_request_startup failed");
        let mut ctx = unsafe { *RequestContext::from_raw(ctx_ptr) };
        unsafe { (*sapi_globals).server_context = std::ptr::null_mut() };
        ctx.http_status_code = 500;
        ctx.output_buffer = b"PHP request startup failed".to_vec();
        return ctx;
    }

    // 4. Get script filename
    let script_filename = {
        let sg = unsafe { &*sapi_globals };
        if !sg.request_info.path_translated.is_null() {
            unsafe { CStr::from_ptr(sg.request_info.path_translated) }
                .to_string_lossy()
                .to_string()
        } else {
            String::new()
        }
    };

    // 5. Execute PHP script with fatal error recovery
    if !script_filename.is_empty() {
        if let Ok(path_cstr) = CString::new(script_filename.as_str()) {
            let catch_result = unsafe {
                try_catch_first(|| {
                    let mut file_handle: zend_file_handle = std::mem::zeroed();
                    zend_stream_init_filename(&raw mut file_handle, path_cstr.as_ptr());
                    php_execute_script(&raw mut file_handle);
                    zend_destroy_file_handle(&raw mut file_handle);
                })
            };
            if catch_result.is_err() {
                error!("PHP bailout caught");
            }
        }
    }

    // 6. Save pointer before shutdown (deactivate nulls server_context)
    let saved_ptr = ctx_ptr;

    // 7. php_request_shutdown
    unsafe { php_request_shutdown(std::ptr::null_mut()) };

    // 8. Free path_translated after shutdown
    {
        let sg = unsafe { &*sapi_globals };
        if !sg.request_info.path_translated.is_null() {
            unsafe { drop(CString::from_raw(sg.request_info.path_translated)) };
        }
    }

    // 9. Reclaim context
    let ctx = unsafe { *RequestContext::from_raw(saved_ptr) };
    tracing::debug!(
        "execute_request done: status={}, output_len={}, headers={}",
        ctx.http_status_code,
        ctx.output_buffer.len(),
        ctx.response_headers.len()
    );
    ctx
}

// ---------------------------------------------------------------------------
// try_catch_first — fatal error recovery via setjmp/longjmp
// ---------------------------------------------------------------------------

/// Implements zend_first_try/zend_catch using this process's executor_globals.
/// Uses setjmp to set up a bailout target. PHP's zend_bailout() calls
/// longjmp back to this point on fatal errors.
///
/// # Safety
/// Must be called from the thread executing PHP.
unsafe fn try_catch_first<F: FnOnce()>(f: F) -> Result<(), ()> {
    unsafe {
        let eg = ext_php_rs::ffi::ext_php_rs_executor_globals();

        // zend_first_try: EG(bailout) = NULL
        (*eg).bailout = std::ptr::null_mut();

        // Save original (NULL after first_try init)
        let orig_bailout = (*eg).bailout;

        // Allocate jmp_buf on stack — jmp_buf = [__jmp_buf_tag; 1]
        let mut jmp_buf: ext_php_rs::ffi::jmp_buf = std::mem::zeroed();

        // Set bailout to our jmp_buf
        (*eg).bailout = jmp_buf.as_mut_ptr() as *mut _;

        if setjmp(jmp_buf.as_mut_ptr()) == 0 {
            // Normal execution path
            f();
            (*eg).bailout = orig_bailout;
            Ok(())
        } else {
            // Bailout occurred — PHP called longjmp here
            (*eg).bailout = orig_bailout;
            Err(())
        }
    }
}

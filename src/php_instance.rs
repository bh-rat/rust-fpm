use std::cell::Cell;
use std::ffi::{c_char, c_int, c_void, CStr, CString};

use anyhow::{Context, Result};
use ext_php_rs::ffi::{
    __jmp_buf_tag, sapi_globals_struct, sapi_module_struct, zend_executor_globals,
    zend_file_handle,
};
use ext_php_rs::types::Zval;
use tracing::{error, info};

use crate::php_sapi::{self, RequestContext};

// ---------------------------------------------------------------------------
// Platform FFI — dlmopen (GNU extension), dlinfo, and setjmp (C standard)
// ---------------------------------------------------------------------------

const LM_ID_NEWLM: libc::c_long = -1;

unsafe extern "C" {
    fn dlmopen(
        lmid: libc::c_long,
        filename: *const libc::c_char,
        flags: libc::c_int,
    ) -> *mut libc::c_void;

    // C's setjmp — takes jmp_buf which decays to __jmp_buf_tag* in function call.
    // PHP uses setjmp/longjmp (HAVE_SIGSETJMP is undefined in our build).
    fn setjmp(env: *mut __jmp_buf_tag) -> c_int;
}

// ---------------------------------------------------------------------------
// Thread-local: current PhpInstance for SAPI callback dispatch
// ---------------------------------------------------------------------------

thread_local! {
    static CURRENT_INSTANCE: Cell<*const PhpInstance> = const { Cell::new(std::ptr::null()) };
}

/// Get the current thread's PhpInstance pointer (set during execute_request).
/// Returns null if no instance is active (falls back to primary in raw_sapi_globals).
pub fn current_instance() -> *const PhpInstance {
    CURRENT_INSTANCE.with(|c| c.get())
}

// ---------------------------------------------------------------------------
// Function pointer types matching PHP's C API
// ---------------------------------------------------------------------------

type FnPhpRequestStartup = unsafe extern "C" fn() -> c_int;
type FnPhpRequestShutdown = unsafe extern "C" fn(*mut c_void);
type FnPhpExecuteScript = unsafe extern "C" fn(*mut zend_file_handle) -> bool;
type FnPhpRegisterVariable =
    unsafe extern "C" fn(*const c_char, *const c_char, *mut Zval);
type FnZendStreamInitFilename =
    unsafe extern "C" fn(*mut zend_file_handle, *const c_char);
type FnZendDestroyFileHandle = unsafe extern "C" fn(*mut zend_file_handle);
type FnPhpModuleStartup =
    unsafe extern "C" fn(*mut sapi_module_struct, *mut c_void) -> c_int;
type FnPhpModuleShutdown = unsafe extern "C" fn();
type FnSapiStartup = unsafe extern "C" fn(*mut sapi_module_struct);
type FnSapiShutdown = unsafe extern "C" fn();
type FnZendSignalStartup = unsafe extern "C" fn();

// Linked symbols from libphp.so (compile-time)
unsafe extern "C" {
    fn php_request_startup() -> c_int;
    fn php_request_shutdown(dummy: *mut c_void);
    fn php_execute_script(primary_file: *mut zend_file_handle) -> bool;
    fn php_register_variable(
        var: *const c_char,
        val: *const c_char,
        track_vars_array: *mut Zval,
    );
    fn zend_stream_init_filename(handle: *mut zend_file_handle, filename: *const c_char);
    fn zend_destroy_file_handle(handle: *mut zend_file_handle);
    fn php_module_shutdown();
}

// ---------------------------------------------------------------------------
// PhpInstance — one per worker thread
// ---------------------------------------------------------------------------

pub struct PhpInstance {
    /// dlmopen handle (null for primary/linked instance)
    handle: *mut c_void,
    /// Whether this is the primary (compile-time linked) instance
    is_primary: bool,
    /// Pointer to this namespace's sapi_globals
    pub sapi_globals: *mut sapi_globals_struct,
    /// Pointer to this namespace's executor_globals
    pub executor_globals: *mut zend_executor_globals,
    /// Owned sapi_module_struct (leaked, lives for process lifetime)
    sapi_module: *mut sapi_module_struct,
    // Resolved function pointers
    pub fn_php_register_variable: FnPhpRegisterVariable,
    fn_php_request_startup: FnPhpRequestStartup,
    fn_php_request_shutdown: FnPhpRequestShutdown,
    fn_php_execute_script: FnPhpExecuteScript,
    fn_zend_stream_init_filename: FnZendStreamInitFilename,
    fn_zend_destroy_file_handle: FnZendDestroyFileHandle,
    fn_php_module_shutdown: FnPhpModuleShutdown,
    fn_sapi_shutdown: FnSapiShutdown,
}

// PhpInstance is created on main thread and moved to exactly one worker thread.
// Each instance's globals are in a separate dlmopen namespace — no sharing.
unsafe impl Send for PhpInstance {}

impl PhpInstance {
    /// Create a PhpInstance wrapping the compile-time linked libphp.so.
    /// Call after `php_sapi::init_sapi()` has initialized the primary PHP.
    pub fn primary(sapi_ptr: *mut sapi_module_struct) -> Self {
        let sapi_globals =
            unsafe { ext_php_rs::ffi::ext_php_rs_sapi_globals() };
        let executor_globals =
            unsafe { ext_php_rs::ffi::ext_php_rs_executor_globals() };

        PhpInstance {
            handle: std::ptr::null_mut(),
            is_primary: true,
            sapi_globals,
            executor_globals,
            sapi_module: sapi_ptr,
            fn_php_register_variable: php_register_variable,
            fn_php_request_startup: php_request_startup,
            fn_php_request_shutdown: php_request_shutdown,
            fn_php_execute_script: php_execute_script,
            fn_zend_stream_init_filename: zend_stream_init_filename,
            fn_zend_destroy_file_handle: zend_destroy_file_handle,
            fn_php_module_shutdown: php_module_shutdown,
            fn_sapi_shutdown: ext_php_rs::ffi::sapi_shutdown,
        }
    }

    /// Create a new PHP instance in an isolated dlmopen namespace.
    /// Each instance gets completely independent globals (heap, sapi, executor).
    ///
    /// Note: OPcache (opcache.so) cannot be loaded in dlmopen'd namespaces due to
    /// a glibc limitation — dlopen inside a dlmopen'd namespace crashes in
    /// add_to_global_resize. OPcache only works on the primary instance.
    pub fn dlmopen(libphp_path: &str, php_ini_path: Option<&str>) -> Result<Self> {
        let c_path = CString::new(libphp_path)
            .context("Invalid libphp path")?;

        // Load libphp.so into a new linker namespace
        let handle = unsafe { dlmopen(LM_ID_NEWLM, c_path.as_ptr(), libc::RTLD_NOW) };
        if handle.is_null() {
            let err = unsafe { CStr::from_ptr(libc::dlerror()) };
            anyhow::bail!("dlmopen failed: {}", err.to_string_lossy());
        }

        let has_shim = false;

        // 2. Resolve all required symbols
        let resolve = |name: &str| -> Result<*mut c_void> {
            let c_name = CString::new(name).unwrap();
            let sym = unsafe { libc::dlsym(handle, c_name.as_ptr()) };
            if sym.is_null() {
                anyhow::bail!("dlsym failed for '{}': symbol not found", name);
            }
            Ok(sym)
        };

        let fn_php_request_startup: FnPhpRequestStartup =
            unsafe { std::mem::transmute(resolve("php_request_startup")?) };
        let fn_php_request_shutdown: FnPhpRequestShutdown =
            unsafe { std::mem::transmute(resolve("php_request_shutdown")?) };
        let fn_php_execute_script: FnPhpExecuteScript =
            unsafe { std::mem::transmute(resolve("php_execute_script")?) };
        let fn_php_register_variable: FnPhpRegisterVariable =
            unsafe { std::mem::transmute(resolve("php_register_variable")?) };
        let fn_zend_stream_init_filename: FnZendStreamInitFilename =
            unsafe { std::mem::transmute(resolve("zend_stream_init_filename")?) };
        let fn_zend_destroy_file_handle: FnZendDestroyFileHandle =
            unsafe { std::mem::transmute(resolve("zend_destroy_file_handle")?) };
        let fn_php_module_startup: FnPhpModuleStartup =
            unsafe { std::mem::transmute(resolve("php_module_startup")?) };
        let fn_php_module_shutdown: FnPhpModuleShutdown =
            unsafe { std::mem::transmute(resolve("php_module_shutdown")?) };
        let fn_sapi_startup: FnSapiStartup =
            unsafe { std::mem::transmute(resolve("sapi_startup")?) };
        let fn_sapi_shutdown: FnSapiShutdown =
            unsafe { std::mem::transmute(resolve("sapi_shutdown")?) };
        let fn_zend_signal_startup: FnZendSignalStartup =
            unsafe { std::mem::transmute(resolve("zend_signal_startup")?) };

        // Resolve data symbols (global variables in this namespace)
        let sapi_globals: *mut sapi_globals_struct =
            resolve("sapi_globals")? as *mut sapi_globals_struct;
        let executor_globals: *mut zend_executor_globals =
            resolve("executor_globals")? as *mut zend_executor_globals;

        // 3. Initialize this namespace's signal handling
        unsafe { fn_zend_signal_startup() };

        // 4. Build sapi_module_struct (same callbacks, no PHP calls)
        // If the shim is loaded, OPcache can safely initialize — the shim
        // redirects SHM operations to the primary's segments.
        // Without shim, skip ini scan to avoid loading zend_extension=opcache.
        let skip_ini_scan = !has_shim;
        let sapi_module = php_sapi::build_sapi_module(php_ini_path, skip_ini_scan);
        // Null out startup/shutdown callbacks — for dlmopen'd instances we manage
        // lifecycle directly via resolved function pointers. If left set, PHP's
        // php_module_shutdown() would call our callback which invokes the LINKED
        // php_module_shutdown, corrupting the primary instance.
        unsafe {
            (*sapi_module).startup = None;
            (*sapi_module).shutdown = None;
        }

        // 5. Register SAPI module with this namespace's PHP
        unsafe { fn_sapi_startup(sapi_module) };

        // 6. Initialize PHP modules in this namespace
        let startup_result =
            unsafe { fn_php_module_startup(sapi_module, std::ptr::null_mut()) };
        if startup_result != ext_php_rs::ffi::ZEND_RESULT_CODE_SUCCESS {
            unsafe { libc::dlclose(handle) };
            anyhow::bail!("php_module_startup failed for dlmopen'd instance");
        }

        info!("dlmopen'd PHP instance created from {}", libphp_path);

        Ok(PhpInstance {
            handle,
            is_primary: false,
            sapi_globals,
            executor_globals,
            sapi_module,
            fn_php_register_variable,
            fn_php_request_startup,
            fn_php_request_shutdown,
            fn_php_execute_script,
            fn_zend_stream_init_filename,
            fn_zend_destroy_file_handle,
            fn_php_module_shutdown,
            fn_sapi_shutdown,
        })
    }

    /// Execute a PHP request using this instance's isolated namespace.
    /// Sets TLS so SAPI callbacks route to the correct globals.
    pub fn execute_request(&self, ctx: RequestContext) -> RequestContext {
        // Set TLS for callback dispatch
        CURRENT_INSTANCE.with(|c| c.set(self as *const _));

        // 1. Set request_info fields (uses raw_sapi_globals() → TLS → self.sapi_globals)
        php_sapi::init_request_info(&ctx);

        // 2. Store context in server_context
        let ctx_ptr = ctx.into_raw();
        unsafe { (*self.sapi_globals).server_context = ctx_ptr };

        // 3. php_request_startup
        let startup_result = unsafe { (self.fn_php_request_startup)() };
        if startup_result != ext_php_rs::ffi::ZEND_RESULT_CODE_SUCCESS {
            error!("php_request_startup failed");
            let mut ctx = unsafe { *RequestContext::from_raw(ctx_ptr) };
            unsafe { (*self.sapi_globals).server_context = std::ptr::null_mut() };
            ctx.http_status_code = 500;
            ctx.output_buffer = b"PHP request startup failed".to_vec();
            CURRENT_INSTANCE.with(|c| c.set(std::ptr::null()));
            return ctx;
        }

        // 4. Get script filename
        let script_filename = {
            let sg = unsafe { &*self.sapi_globals };
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
                let catch_result = unsafe { self.try_catch_first(|| {
                    let mut file_handle: zend_file_handle = std::mem::zeroed();
                    (self.fn_zend_stream_init_filename)(
                        &raw mut file_handle,
                        path_cstr.as_ptr(),
                    );
                    (self.fn_php_execute_script)(&raw mut file_handle);
                    (self.fn_zend_destroy_file_handle)(&raw mut file_handle);
                }) };
                if catch_result.is_err() {
                    error!("PHP bailout caught");
                }
            }
        }

        // 6. Save pointer before shutdown (deactivate nulls server_context)
        let saved_ptr = ctx_ptr;

        // 7. php_request_shutdown
        unsafe { (self.fn_php_request_shutdown)(std::ptr::null_mut()) };

        // 8. Free path_translated after shutdown
        {
            let sg = unsafe { &*self.sapi_globals };
            if !sg.request_info.path_translated.is_null() {
                unsafe { drop(CString::from_raw(sg.request_info.path_translated)) };
            }
        }

        // 9. Clear TLS
        CURRENT_INSTANCE.with(|c| c.set(std::ptr::null()));

        // 10. Reclaim context
        let ctx = unsafe { *RequestContext::from_raw(saved_ptr) };
        tracing::debug!(
            "execute_request done: status={}, output_len={}, headers={}",
            ctx.http_status_code,
            ctx.output_buffer.len(),
            ctx.response_headers.len()
        );
        ctx
    }

    /// Implements zend_first_try/zend_catch using this instance's executor_globals.
    /// Uses setjmp to set up a bailout target. PHP's zend_bailout() calls
    /// longjmp back to this point on fatal errors.
    ///
    /// # Safety
    /// Must be called on the thread that owns this PhpInstance.
    unsafe fn try_catch_first<F: FnOnce()>(&self, f: F) -> std::result::Result<(), ()> {
        unsafe {
            let eg = self.executor_globals;

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
}

impl Drop for PhpInstance {
    fn drop(&mut self) {
        if !self.is_primary && !self.handle.is_null() {
            info!("Shutting down dlmopen'd PHP instance");
            unsafe {
                (self.fn_php_module_shutdown)();
                (self.fn_sapi_shutdown)();
                libc::dlclose(self.handle);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Utility: discover the path to the loaded libphp.so at runtime
// ---------------------------------------------------------------------------

/// Find the filesystem path to the currently loaded libphp.so.
/// Uses dladdr() on a known PHP symbol to find its containing library.
pub fn find_libphp_path() -> String {
    let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
    // Use php_request_startup as the probe symbol — it's definitely in libphp.so
    let symbol = php_request_startup as *const c_void;
    let result = unsafe { libc::dladdr(symbol, &mut info) };
    if result != 0 && !info.dli_fname.is_null() {
        let path = unsafe { CStr::from_ptr(info.dli_fname) }
            .to_string_lossy()
            .to_string();
        info!("Discovered libphp.so path: {}", path);
        path
    } else {
        let fallback = "/usr/local/lib/libphp.so".to_string();
        info!("dladdr failed, using fallback: {}", fallback);
        fallback
    }
}

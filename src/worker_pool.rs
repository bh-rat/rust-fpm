use std::sync::{Arc, Mutex};
use std::thread;

use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info};

use crate::php_instance::PhpInstance;
use crate::php_sapi::RequestContext;

struct WorkerRequest {
    context: RequestContext,
    response_tx: oneshot::Sender<RequestContext>,
}

/// Fixed-size pool of OS threads, each with its own isolated PHP instance.
/// Worker 0 uses the primary (compile-time linked) libphp.so with OPcache.
/// Workers 1+ each get a dlmopen'd copy with independent globals (no OPcache).
pub struct WorkerPool {
    sender: mpsc::Sender<WorkerRequest>,
    num_workers: usize,
    _handles: Vec<thread::JoinHandle<()>>,
}

/// Parameters needed by workers to create dlmopen'd PHP instances.
struct WorkerFactory {
    libphp_path: String,
    php_ini_path: Option<String>,
}

impl WorkerPool {
    /// Create a new worker pool.
    /// Worker 0 receives the primary PhpInstance, workers 1+ create dlmopen'd instances.
    pub fn new(
        num_workers: usize,
        primary: PhpInstance,
        libphp_path: String,
        php_ini_path: Option<String>,
    ) -> Self {
        assert!(num_workers > 0, "Need at least 1 worker");
        if num_workers > 16 {
            panic!(
                "pm_max_children={} exceeds glibc dlmopen limit of 16 namespaces per process",
                num_workers
            );
        }

        let (tx, rx) = mpsc::channel::<WorkerRequest>(num_workers * 2);
        let rx = Arc::new(Mutex::new(rx));
        let mut handles = Vec::with_capacity(num_workers);

        // Wrap primary in Arc<Mutex> so we can move it to worker 0's thread
        let primary = Arc::new(Mutex::new(Some(primary)));

        let factory = Arc::new(WorkerFactory {
            libphp_path,
            php_ini_path,
        });

        for id in 0..num_workers {
            let rx = rx.clone();
            let primary = primary.clone();
            let factory = factory.clone();

            let handle = thread::Builder::new()
                .name(format!("php-worker-{}", id))
                .stack_size(16 * 1024 * 1024) // 16MB stack — PHP 8.3+ needs it
                .spawn(move || {
                    worker_loop(id, primary, factory, rx);
                })
                .expect("Failed to spawn worker thread");
            handles.push(handle);
        }

        info!(
            "Worker pool started with {} workers (1 primary + {} dlmopen'd)",
            num_workers,
            num_workers.saturating_sub(1)
        );
        WorkerPool {
            sender: tx,
            num_workers,
            _handles: handles,
        }
    }

    /// Submit a request to the pool and wait for the result.
    pub async fn execute(&self, ctx: RequestContext) -> anyhow::Result<RequestContext> {
        let (tx, rx) = oneshot::channel();
        self.sender
            .send(WorkerRequest {
                context: ctx,
                response_tx: tx,
            })
            .await
            .map_err(|_| anyhow::anyhow!("Worker pool channel closed"))?;

        rx.await
            .map_err(|_| anyhow::anyhow!("Worker dropped response channel"))
    }

    pub fn num_workers(&self) -> usize {
        self.num_workers
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        info!("Worker pool shutting down ({} workers)", self.num_workers);
    }
}

/// Main loop for each worker thread.
fn worker_loop(
    id: usize,
    primary: Arc<Mutex<Option<PhpInstance>>>,
    factory: Arc<WorkerFactory>,
    rx: Arc<Mutex<mpsc::Receiver<WorkerRequest>>>,
) {
    info!("Worker {} starting", id);

    // Create or take the PHP instance for this worker.
    // dlmopen'd instances MUST be created on their worker thread because
    // php_module_startup() initializes thread-local state (TLS) that is
    // only valid on the thread that called it.
    let instance = if id == 0 {
        // Worker 0 takes the primary (linked) instance
        primary.lock().unwrap().take()
            .expect("Primary PhpInstance already taken")
    } else {
        // Workers 1+ create dlmopen'd instances on their own thread
        match PhpInstance::dlmopen(&factory.libphp_path, factory.php_ini_path.as_deref()) {
            Ok(inst) => {
                info!("Worker {} created dlmopen'd PHP instance", id);
                inst
            }
            Err(e) => {
                error!("Worker {} failed to create PHP instance: {:?}", id, e);
                return;
            }
        }
    };

    loop {
        // Lock mutex only to receive — released before PHP execution
        let work = {
            let mut guard = rx.lock().unwrap();
            guard.blocking_recv()
        };

        match work {
            Some(req) => {
                debug!("Worker {} processing request", id);

                // Execute PHP with panic recovery
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    instance.execute_request(req.context)
                }));

                match result {
                    Ok(ctx) => {
                        req.response_tx.send(ctx).ok();
                    }
                    Err(panic_info) => {
                        error!("Worker {} panic: {:?}", id, panic_info);
                        let error_ctx =
                            RequestContext::error_response(500, "Internal Server Error");
                        req.response_tx.send(error_ctx).ok();
                    }
                }

                debug!("Worker {} done", id);
            }
            None => {
                // Channel closed — shutdown
                info!("Worker {} shutting down", id);
                break;
            }
        }
    }

    // instance drops here — dlmopen'd instances call php_module_shutdown + dlclose
    drop(instance);
    info!("Worker {} exited", id);
}

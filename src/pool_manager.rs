use std::ffi::CString;
use std::sync::Arc;

use anyhow::{Context, Result};
use nix::sys::signal::{self, Signal};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{self, ForkResult, Pid};
use tracing::{error, info, warn};

use crate::config::{Config, PoolConfig};
use crate::pool_manager::signals::SignalState;

/// Run in multi-pool mode: fork a child process per pool, each dropping to its own user.
pub fn run(config: Config) -> Result<()> {
    let mut children: Vec<ChildProcess> = Vec::new();

    // Write PID file for master
    if let Some(ref pid_path) = config.global.pid {
        let pid = std::process::id();
        std::fs::write(pid_path, pid.to_string())
            .with_context(|| format!("Failed to write PID file: {}", pid_path))?;
        info!("Master PID {} written to {}", pid, pid_path);
    }

    for pool_config in &config.pools {
        let user = pool_config
            .user
            .as_deref()
            .unwrap_or_else(|| {
                warn!("Pool '{}' has no user set, running as current user", pool_config.name);
                ""
            });

        info!(
            "Forking pool '{}': listen={} user={} workers={}",
            pool_config.name,
            pool_config.listen,
            if user.is_empty() { "(current)" } else { user },
            pool_config.pm_max_children
        );

        match unsafe { unistd::fork() }.context("fork() failed")? {
            ForkResult::Parent { child } => {
                info!("Pool '{}' forked as PID {}", pool_config.name, child);
                children.push(ChildProcess {
                    pid: child,
                    pool_name: pool_config.name.clone(),
                });
            }
            ForkResult::Child => {
                // Child process — run this pool
                let result = run_child_pool(pool_config);
                if let Err(e) = &result {
                    error!("Pool '{}' failed: {:?}", pool_config.name, e);
                }
                std::process::exit(if result.is_ok() { 0 } else { 1 });
            }
        }
    }

    // Master process: monitor children, handle signals
    master_loop(&mut children, &config)
}

struct ChildProcess {
    pid: Pid,
    pool_name: String,
}

/// Master process: wait for children, forward signals.
fn master_loop(children: &mut Vec<ChildProcess>, config: &Config) -> Result<()> {
    info!(
        "Master process running, monitoring {} children",
        children.len()
    );

    let signals = SignalState::install()?;

    loop {
        // Check for pending signals
        if signals.got_sigterm() || signals.got_sigint() {
            let sig_name = if signals.got_sigterm() { "SIGTERM" } else { "SIGINT" };
            info!("Master received {}, shutting down children", sig_name);
            forward_signal(children, Signal::SIGTERM);

            // Wait for children to exit (with timeout)
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while !children.is_empty() && std::time::Instant::now() < deadline {
                reap_children(children);
                if !children.is_empty() {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }

            // Force kill any remaining
            if !children.is_empty() {
                warn!("Force killing {} remaining children", children.len());
                forward_signal(children, Signal::SIGKILL);
                std::thread::sleep(std::time::Duration::from_millis(500));
                reap_children(children);
            }

            break;
        }

        // Reap any exited children
        reap_children(children);

        if children.is_empty() {
            info!("All children have exited");
            break;
        }

        // Sleep briefly to avoid busy-waiting
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    // Cleanup PID file
    if let Some(ref pid_path) = config.global.pid {
        let _ = std::fs::remove_file(pid_path);
    }

    info!("Master exiting");
    Ok(())
}

/// Non-blocking reap of exited children.
fn reap_children(children: &mut Vec<ChildProcess>) {
    children.retain(|child| {
        match waitpid(Some(child.pid), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Exited(pid, status)) => {
                info!("Pool '{}' (PID {}) exited with status {}", child.pool_name, pid, status);
                false // remove from list
            }
            Ok(WaitStatus::Signaled(pid, signal, _)) => {
                warn!(
                    "Pool '{}' (PID {}) killed by signal {:?}",
                    child.pool_name, pid, signal
                );
                false
            }
            Ok(WaitStatus::StillAlive) => true, // keep in list
            Ok(_) => true,
            Err(e) => {
                error!("waitpid error for pool '{}': {}", child.pool_name, e);
                false
            }
        }
    });
}

fn forward_signal(children: &[ChildProcess], signal: Signal) {
    for child in children {
        if let Err(e) = signal::kill(child.pid, signal) {
            error!(
                "Failed to send {:?} to pool '{}' (PID {}): {}",
                signal, child.pool_name, child.pid, e
            );
        }
    }
}

/// Child process entry point: bind socket, drop privileges, init PHP, run pool.
fn run_child_pool(pool_config: &PoolConfig) -> Result<()> {
    // 1. Bind socket BEFORE dropping privileges (path may be root-owned)
    let _ = std::fs::remove_file(&pool_config.listen);
    let listener = std::os::unix::net::UnixListener::bind(&pool_config.listen)
        .with_context(|| format!("Failed to bind socket: {}", pool_config.listen))?;

    // 2. Set socket ownership and permissions
    set_socket_permissions(pool_config)?;

    // 3. Drop privileges if user is configured
    if let Some(ref user) = pool_config.user {
        let group = pool_config.group.as_deref().unwrap_or(user);
        drop_privileges(user, group)
            .with_context(|| format!("Failed to drop privileges to {}:{}", user, group))?;
        info!(
            "Pool '{}': dropped privileges to {}:{}",
            pool_config.name, user, group
        );
    }

    // 4. Initialize PHP SAPI (in child, after privilege drop)
    let sapi_ptr = crate::php_sapi::init_sapi(pool_config.php_ini.as_deref());
    info!("Pool '{}': PHP SAPI initialized", pool_config.name);

    // 5. Create primary PhpInstance and discover libphp path for dlmopen
    let primary = crate::php_instance::PhpInstance::primary(sapi_ptr);
    let libphp_path = crate::php_instance::find_libphp_path();

    // 6. Create worker pool (worker 0 = primary, workers 1+ = dlmopen'd)
    let pool = Arc::new(crate::worker_pool::WorkerPool::new(
        pool_config.pm_max_children,
        primary,
        libphp_path,
        pool_config.php_ini.clone(),
    ));
    info!(
        "Pool '{}': {} workers ready",
        pool_config.name,
        pool.num_workers()
    );

    // 7. Create Tokio runtime (after fork, after privilege drop)
    let rt = tokio::runtime::Runtime::new()
        .context("Failed to create Tokio runtime in child")?;

    // 8. Run FastCGI listener on the pre-bound socket
    let result = rt.block_on(crate::fastcgi::serve_on_listener(listener, pool));

    // 9. Cleanup
    crate::php_sapi::shutdown_sapi();

    result
}

fn set_socket_permissions(pool_config: &PoolConfig) -> Result<()> {
    // chown socket to listen_owner:listen_group
    if pool_config.listen_owner.is_some() || pool_config.listen_group.is_some() {
        let uid = pool_config
            .listen_owner
            .as_deref()
            .map(|name| {
                unistd::User::from_name(name)
                    .ok()
                    .flatten()
                    .map(|u| u.uid)
            })
            .flatten();
        let gid = pool_config
            .listen_group
            .as_deref()
            .map(|name| {
                unistd::Group::from_name(name)
                    .ok()
                    .flatten()
                    .map(|g| g.gid)
            })
            .flatten();
        unistd::chown(pool_config.listen.as_str(), uid, gid)
            .with_context(|| format!("Failed to chown socket {}", pool_config.listen))?;
    }

    // chmod socket
    let mode_str = pool_config.listen_mode.as_deref().unwrap_or("0660");
    let mode_bits = u32::from_str_radix(mode_str.trim_start_matches('0'), 8)
        .unwrap_or(0o660);
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        &pool_config.listen,
        std::fs::Permissions::from_mode(mode_bits),
    )?;

    Ok(())
}

fn drop_privileges(user: &str, group: &str) -> Result<()> {
    let usr = unistd::User::from_name(user)
        .with_context(|| format!("Failed to look up user '{}'", user))?
        .ok_or_else(|| anyhow::anyhow!("User '{}' not found", user))?;
    let grp = unistd::Group::from_name(group)
        .with_context(|| format!("Failed to look up group '{}'", group))?
        .ok_or_else(|| anyhow::anyhow!("Group '{}' not found", group))?;

    // Must set supplementary groups before dropping root
    let c_user = CString::new(user).context("Invalid username")?;
    unistd::initgroups(&c_user, grp.gid).context("initgroups failed")?;

    // Drop group privileges
    unistd::setgid(grp.gid).context("setgid failed")?;

    // Drop user privileges (point of no return)
    unistd::setuid(usr.uid).context("setuid failed")?;

    // Verify we actually dropped privileges
    if unistd::geteuid().is_root() {
        anyhow::bail!("Failed to drop root privileges — still running as root");
    }

    Ok(())
}

/// Signal handling for the master process using atomic flags.
mod signals {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::ffi::c_int;

    use nix::sys::signal::{self, SaFlags, SigAction, SigHandler, SigSet, Signal};

    static GOT_SIGTERM: AtomicBool = AtomicBool::new(false);
    static GOT_SIGINT: AtomicBool = AtomicBool::new(false);

    extern "C" fn handle_sigterm(_: c_int) {
        GOT_SIGTERM.store(true, Ordering::SeqCst);
    }

    extern "C" fn handle_sigint(_: c_int) {
        GOT_SIGINT.store(true, Ordering::SeqCst);
    }

    pub struct SignalState;

    impl SignalState {
        pub fn install() -> anyhow::Result<Self> {
            let sa_term = SigAction::new(
                SigHandler::Handler(handle_sigterm),
                SaFlags::SA_RESTART,
                SigSet::empty(),
            );
            let sa_int = SigAction::new(
                SigHandler::Handler(handle_sigint),
                SaFlags::SA_RESTART,
                SigSet::empty(),
            );
            unsafe {
                signal::sigaction(Signal::SIGTERM, &sa_term)?;
                signal::sigaction(Signal::SIGINT, &sa_int)?;
            }
            Ok(SignalState)
        }

        pub fn got_sigterm(&self) -> bool {
            GOT_SIGTERM.load(Ordering::SeqCst)
        }

        pub fn got_sigint(&self) -> bool {
            GOT_SIGINT.load(Ordering::SeqCst)
        }
    }
}

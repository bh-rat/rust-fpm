use std::ffi::CString;
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::time::Instant;

use anyhow::{Context, Result};
use nix::sys::signal::{self, Signal};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{self, ForkResult, Pid};
use tracing::{error, info, warn};

use crate::config::{Config, PoolConfig};
use crate::pool_manager::signals::SignalState;

/// Run the fork-based worker model.
/// Master forks pm_max_children workers per pool, each inheriting the PHP
/// context (including OPcache SHM) from the master process.
pub fn run(
    config: Config,
    pool_listeners: Vec<std::os::unix::net::UnixListener>,
) -> Result<()> {
    let mut children: Vec<WorkerChild> = Vec::new();

    // Write PID file for master
    if let Some(ref pid_path) = config.global.pid {
        let pid = std::process::id();
        std::fs::write(pid_path, pid.to_string())
            .with_context(|| format!("Failed to write PID file: {}", pid_path))?;
        info!("Master PID {} written to {}", pid, pid_path);
    }

    // Fork workers for each pool
    for (pool_idx, pool_config) in config.pools.iter().enumerate() {
        let socket_fd = pool_listeners[pool_idx].as_raw_fd();

        let user = pool_config.user.as_deref().unwrap_or("");

        info!(
            "Forking {} workers for pool '{}': listen={} user={}",
            pool_config.pm_max_children,
            pool_config.name,
            pool_config.listen,
            if user.is_empty() { "(current)" } else { user },
        );

        for worker_id in 0..pool_config.pm_max_children {
            let pid = spawn_worker(pool_config, pool_idx, worker_id, socket_fd)?;
            children.push(WorkerChild {
                pid,
                pool_index: pool_idx,
                worker_id,
                spawn_time: Instant::now(),
            });
        }
    }

    info!(
        "Master process running, {} workers forked across {} pool(s)",
        children.len(),
        config.pools.len()
    );

    // Master process: monitor children, handle signals, respawn
    master_loop(&mut children, &config, &pool_listeners)
}

struct WorkerChild {
    pid: Pid,
    pool_index: usize,
    worker_id: usize,
    spawn_time: Instant,
}

/// Fork a single worker process.
fn spawn_worker(
    pool: &PoolConfig,
    pool_idx: usize,
    worker_id: usize,
    socket_fd: i32,
) -> Result<Pid> {
    match unsafe { unistd::fork() }.context("fork() failed")? {
        ForkResult::Parent { child } => {
            info!(
                "Pool '{}' worker {} forked as PID {}",
                pool.name, worker_id, child
            );
            Ok(child)
        }
        ForkResult::Child => {
            // Drop privileges if configured
            if let Some(ref user) = pool.user {
                let group = pool.group.as_deref().unwrap_or(user);
                if let Err(e) = drop_privileges(user, group) {
                    error!(
                        "Pool '{}' worker {}: failed to drop privileges: {:?}",
                        pool.name, worker_id, e
                    );
                    std::process::exit(1);
                }
                info!(
                    "Pool '{}' worker {}: dropped privileges to {}:{}",
                    pool.name, worker_id, user, group
                );
            }

            // Run worker accept loop (never returns normally)
            let exit_code = run_worker(socket_fd, &pool.name, worker_id);
            std::process::exit(exit_code);
        }
    }
}

/// Worker process entry point. Creates Tokio runtime and runs FastCGI accept loop.
fn run_worker(socket_fd: i32, pool_name: &str, worker_id: usize) -> i32 {
    info!("Pool '{}' worker {} starting", pool_name, worker_id);

    // Create Tokio runtime: 1 async thread for IO, 1 blocking thread for PHP
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .max_blocking_threads(1)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            error!("Failed to create Tokio runtime: {:?}", e);
            return 1;
        }
    };

    // Reconstruct UnixListener from inherited fd (fork duplicates fd table)
    let std_listener = unsafe { std::os::unix::net::UnixListener::from_raw_fd(socket_fd) };

    let result = rt.block_on(crate::fastcgi::serve_worker(std_listener));

    if let Err(e) = result {
        error!(
            "Pool '{}' worker {} error: {:?}",
            pool_name, worker_id, e
        );
        return 1;
    }

    0
}

/// Master process: wait for children, forward signals, respawn crashed workers.
fn master_loop(
    children: &mut Vec<WorkerChild>,
    config: &Config,
    pool_listeners: &[std::os::unix::net::UnixListener],
) -> Result<()> {
    let signals = SignalState::install()?;

    loop {
        // Check for shutdown signals
        if signals.got_sigterm() || signals.got_sigint() {
            let sig_name = if signals.got_sigterm() {
                "SIGTERM"
            } else {
                "SIGINT"
            };
            info!("Master received {}, shutting down workers", sig_name);
            forward_signal(children, Signal::SIGTERM);

            // Wait for children to exit (with timeout)
            let deadline = Instant::now() + std::time::Duration::from_secs(30);
            while !children.is_empty() && Instant::now() < deadline {
                reap_children(children);
                if !children.is_empty() {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }

            // Force kill any remaining
            if !children.is_empty() {
                warn!("Force killing {} remaining workers", children.len());
                forward_signal(children, Signal::SIGKILL);
                std::thread::sleep(std::time::Duration::from_millis(500));
                reap_children(children);
            }

            break;
        }

        // Reap any exited children
        let before = children.len();
        reap_children(children);

        // Respawn workers that died (unless shutting down)
        if children.len() < before {
            for pool_idx in 0..config.pools.len() {
                let alive = children
                    .iter()
                    .filter(|c| c.pool_index == pool_idx)
                    .count();
                let target = config.pools[pool_idx].pm_max_children;

                for worker_id in alive..target {
                    let socket_fd = pool_listeners[pool_idx].as_raw_fd();
                    match spawn_worker(&config.pools[pool_idx], pool_idx, worker_id, socket_fd) {
                        Ok(pid) => {
                            children.push(WorkerChild {
                                pid,
                                pool_index: pool_idx,
                                worker_id,
                                spawn_time: Instant::now(),
                            });
                        }
                        Err(e) => {
                            error!(
                                "Failed to respawn worker for pool '{}': {:?}",
                                config.pools[pool_idx].name, e
                            );
                        }
                    }
                }
            }
        }

        if children.is_empty() {
            info!("All workers have exited");
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
fn reap_children(children: &mut Vec<WorkerChild>) {
    children.retain(|child| {
        match waitpid(Some(child.pid), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Exited(pid, status)) => {
                if status == 0 {
                    info!("Worker PID {} exited normally", pid);
                } else {
                    warn!("Worker PID {} exited with status {}", pid, status);
                }
                false // remove from list
            }
            Ok(WaitStatus::Signaled(pid, signal, _)) => {
                warn!("Worker PID {} killed by signal {:?}", pid, signal);
                false
            }
            Ok(WaitStatus::StillAlive) => true, // keep in list
            Ok(_) => true,
            Err(e) => {
                error!("waitpid error for PID {}: {}", child.pid, e);
                false
            }
        }
    });
}

fn forward_signal(children: &[WorkerChild], signal: Signal) {
    for child in children {
        if let Err(e) = signal::kill(child.pid, signal) {
            error!(
                "Failed to send {:?} to PID {}: {}",
                signal, child.pid, e
            );
        }
    }
}

/// Set socket ownership and permissions. Called from main.rs before fork.
pub fn set_socket_permissions(pool_config: &PoolConfig) -> Result<()> {
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

    // chmod socket — default to 0666 so NGINX (www-data) can connect
    let mode_str = pool_config.listen_mode.as_deref().unwrap_or("0666");
    let mode_bits =
        u32::from_str_radix(mode_str.trim_start_matches('0'), 8).unwrap_or(0o660);
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
    use std::ffi::c_int;
    use std::sync::atomic::{AtomicBool, Ordering};

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

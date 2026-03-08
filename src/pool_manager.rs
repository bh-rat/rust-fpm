use anyhow::{Context, Result};
use nix::unistd;
use std::ffi::CString;
use tokio::signal;
use tracing::info;

use crate::config::{Config, PoolConfig};

/// Run the thread-based model (ZTS).
/// Single process, Tokio runtime with N blocking threads for PHP.
/// Each pool gets its own accept loop task.
pub async fn run_threaded(
    config: Config,
    pool_listeners: Vec<std::os::unix::net::UnixListener>,
) -> Result<()> {
    // Write PID file
    if let Some(ref pid_path) = config.global.pid {
        let pid = std::process::id();
        std::fs::write(pid_path, pid.to_string())
            .with_context(|| format!("Failed to write PID file: {}", pid_path))?;
        info!("PID {} written to {}", pid, pid_path);
    }

    // Drop privileges if configured (do this once for the process)
    if let Some(pool) = config.pools.first() {
        if let Some(ref user) = pool.user {
            let group = pool.group.as_deref().unwrap_or(user);
            drop_privileges(user, group)?;
            info!("Dropped privileges to {}:{}", user, group);
        }
    }

    // Spawn accept loop for each pool
    let mut handles = Vec::new();
    for (idx, listener) in pool_listeners.into_iter().enumerate() {
        let pool_name = config.pools[idx].name.clone();
        let handle = tokio::spawn(async move {
            if let Err(e) = crate::fastcgi::serve(listener).await {
                tracing::error!("Pool '{}' error: {:?}", pool_name, e);
            }
        });
        handles.push(handle);
    }

    info!(
        "Serving {} pool(s), {} total blocking threads",
        config.pools.len(),
        config.pools.iter().map(|p| p.pm_max_children).sum::<usize>()
    );

    // Wait for shutdown signal
    signal::ctrl_c().await?;
    info!("Received shutdown signal");

    // Cleanup PID file
    if let Some(ref pid_path) = config.global.pid {
        let _ = std::fs::remove_file(pid_path);
    }

    Ok(())
}

/// Set socket ownership and permissions.
pub fn set_socket_permissions(pool_config: &PoolConfig) -> Result<()> {
    if pool_config.listen_owner.is_some() || pool_config.listen_group.is_some() {
        let uid = pool_config
            .listen_owner
            .as_deref()
            .and_then(|name| unistd::User::from_name(name).ok().flatten().map(|u| u.uid));
        let gid = pool_config
            .listen_group
            .as_deref()
            .and_then(|name| {
                unistd::Group::from_name(name)
                    .ok()
                    .flatten()
                    .map(|g| g.gid)
            });
        unistd::chown(pool_config.listen.as_str(), uid, gid)
            .with_context(|| format!("Failed to chown socket {}", pool_config.listen))?;
    }

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

    let c_user = CString::new(user).context("Invalid username")?;
    unistd::initgroups(&c_user, grp.gid).context("initgroups failed")?;
    unistd::setgid(grp.gid).context("setgid failed")?;
    unistd::setuid(usr.uid).context("setuid failed")?;

    if unistd::geteuid().is_root() {
        anyhow::bail!("Failed to drop root privileges — still running as root");
    }

    Ok(())
}

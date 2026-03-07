use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Default)]
pub struct GlobalConfig {
    pub pid: Option<String>,
    pub error_log: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PoolConfig {
    pub name: String,
    pub listen: String,
    pub user: Option<String>,
    pub group: Option<String>,
    pub listen_owner: Option<String>,
    pub listen_group: Option<String>,
    pub listen_mode: Option<String>,
    pub pm_max_children: usize,
    pub php_ini: Option<String>,
    pub request_terminate_timeout: Option<String>,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            name: "www".into(),
            listen: "/var/run/php-fpm.sock".into(),
            user: None,
            group: None,
            listen_owner: None,
            listen_group: None,
            listen_mode: None,
            pm_max_children: 4,
            php_ini: None,
            request_terminate_timeout: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub global: GlobalConfig,
    pub pools: Vec<PoolConfig>,
}

/// Raw TOML pool section for deserialization.
/// Unused fields are kept for php-fpm config compatibility.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct RawPoolConfig {
    listen: Option<String>,
    user: Option<String>,
    group: Option<String>,
    listen_owner: Option<String>,
    listen_group: Option<String>,
    listen_mode: Option<String>,
    pm_max_children: Option<usize>,
    php_ini: Option<String>,
    request_terminate_timeout: Option<String>,
    // Ignored php-fpm keys for compatibility
    #[serde(default)]
    pm: Option<String>,
    #[serde(default)]
    pm_start_servers: Option<usize>,
    #[serde(default)]
    pm_min_spare_servers: Option<usize>,
    #[serde(default)]
    pm_max_spare_servers: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct RawGlobalConfig {
    pid: Option<String>,
    error_log: Option<String>,
}

impl Config {
    /// Load config from CLI args, optionally loading a TOML config file.
    pub fn load() -> Result<Self> {
        let args: Vec<String> = std::env::args().collect();
        let mut config_path: Option<String> = None;
        let mut cli_listen: Option<String> = None;
        let mut cli_workers: Option<usize> = None;
        let mut cli_php_ini: Option<String> = None;

        let mut i = 1;
        while i < args.len() {
            match args[i].as_str() {
                "--config" | "-c" if i + 1 < args.len() => {
                    i += 1;
                    config_path = Some(args[i].clone());
                }
                "--listen" if i + 1 < args.len() => {
                    i += 1;
                    cli_listen = Some(args[i].clone());
                }
                "--workers" if i + 1 < args.len() => {
                    i += 1;
                    cli_workers = Some(args[i].parse().unwrap_or(4));
                }
                "--php-ini" if i + 1 < args.len() => {
                    i += 1;
                    cli_php_ini = Some(args[i].clone());
                }
                _ => {}
            }
            i += 1;
        }

        if let Some(path) = config_path {
            let mut config = Self::from_toml_file(&path)?;
            // CLI args override config file values for the first pool
            if let Some(pool) = config.pools.first_mut() {
                if let Some(listen) = cli_listen {
                    pool.listen = listen;
                }
                if let Some(workers) = cli_workers {
                    pool.pm_max_children = workers;
                }
                if let Some(ini) = cli_php_ini {
                    pool.php_ini = Some(ini);
                }
            }
            Ok(config)
        } else {
            // No config file — synthesize single pool from CLI args
            let pool = PoolConfig {
                listen: cli_listen.unwrap_or_else(|| "/var/run/php-fpm.sock".into()),
                pm_max_children: cli_workers.unwrap_or(4),
                php_ini: cli_php_ini,
                ..Default::default()
            };
            Ok(Config {
                global: GlobalConfig::default(),
                pools: vec![pool],
            })
        }
    }

    fn from_toml_file(path: &str) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path))?;
        let table: HashMap<String, toml::Value> = toml::from_str(&content)
            .with_context(|| format!("Failed to parse config file: {}", path))?;

        // Extract [global] section
        let global = if let Some(val) = table.get("global") {
            let raw: RawGlobalConfig = val.clone().try_into()
                .context("Invalid [global] section")?;
            GlobalConfig {
                pid: raw.pid,
                error_log: raw.error_log,
            }
        } else {
            GlobalConfig::default()
        };

        // Every other section is a pool
        let mut pools = Vec::new();
        for (name, val) in &table {
            if name == "global" {
                continue;
            }
            let raw: RawPoolConfig = val.clone().try_into()
                .with_context(|| format!("Invalid pool section [{}]", name))?;
            pools.push(PoolConfig {
                name: name.clone(),
                listen: raw.listen.unwrap_or_else(|| format!("/var/run/php-fpm-{}.sock", name)),
                user: raw.user,
                group: raw.group,
                listen_owner: raw.listen_owner,
                listen_group: raw.listen_group,
                listen_mode: raw.listen_mode,
                pm_max_children: raw.pm_max_children.unwrap_or(4),
                php_ini: raw.php_ini,
                request_terminate_timeout: raw.request_terminate_timeout,
            });
        }

        if pools.is_empty() {
            anyhow::bail!("Config file has no pool sections");
        }

        Ok(Config { global, pools })
    }
}

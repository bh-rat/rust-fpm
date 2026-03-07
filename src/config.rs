/// Minimal Phase 1 config — hardcoded defaults, no file parsing
pub struct Config {
    pub listen: String,
    pub php_ini_path: Option<String>,
    pub max_children: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: "/var/run/php-fpm.sock".into(),
            php_ini_path: None,
            max_children: 1,
        }
    }
}

impl Config {
    pub fn from_args() -> Self {
        let mut config = Config::default();
        let args: Vec<String> = std::env::args().collect();
        let mut i = 1;
        while i < args.len() {
            match args[i].as_str() {
                "--listen" if i + 1 < args.len() => {
                    i += 1;
                    config.listen = args[i].clone();
                }
                "--php-ini" if i + 1 < args.len() => {
                    i += 1;
                    config.php_ini_path = Some(args[i].clone());
                }
                "--workers" if i + 1 < args.len() => {
                    i += 1;
                    config.max_children = args[i].parse().unwrap_or(1);
                }
                _ => {}
            }
            i += 1;
        }
        config
    }
}

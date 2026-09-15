use clap::Parser;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::path::PathBuf;
use std::time::Duration;

/// Minimal CLI: just the config file path and optional validation flag.
#[derive(Parser, Debug)]
#[command(name = "cache-aware-router")]
#[command(about = "Minimal cache-aware reverse proxy for vLLM services")]
pub struct CliArgs {
    /// Path to YAML configuration file. Repeatable: files are layered in the
    /// order given, later files win (see `AppConfig::load`).
    #[arg(short, long, default_values = &["config.yaml"])]
    pub config: Vec<PathBuf>,

    /// Validate config and exit without starting the server
    #[arg(long)]
    pub config_check: bool,
}

/// Root config deserialized from YAML.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    #[serde(default)]
    pub server: ServerConfig,

    /// Worker configs — required, no default.
    pub workers: Vec<WorkerEntry>,

    #[serde(default)]
    pub cache: CacheSection,

    #[serde(default)]
    pub health: HealthSection,

    #[serde(default)]
    pub proxy: ProxySection,

    #[serde(default)]
    pub circuit_breaker: CircuitBreakerSection,

    #[serde(default)]
    pub logging: LoggingSection,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CacheSection {
    pub threshold: f32,
    pub match_abs_threshold: usize,
    pub balance_abs_threshold: usize,
    pub balance_rel_threshold: f32,
    pub eviction_interval_secs: u64,
    pub max_tree_size: usize,
    pub daily_cleanup_hour_utc: i32,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct HealthSection {
    pub endpoint: String,
    pub interval_secs: u64,
    pub failure_threshold: u32,
    pub success_threshold: u32,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProxySection {
    pub max_retries: u32,
    pub initial_backoff_ms: u64,
    pub max_backoff_ms: u64,
    pub backoff_multiplier: f32,
    pub jitter_factor: f32,
    pub request_timeout_secs: u64,
    /// 仅约束「建立连接」耗时(TCP+TLS),与整体 request_timeout 独立。
    /// k8s 死 pod IP=黑洞(SYN 被丢)时,不设则请求挂 ~30s(reqwest 内部连接超时;OS 默认更长
    /// ~130s 但被 reqwest 先截断,实测 30s)。设 2s 快失败,不影响长流式生成(与整体 request_timeout 独立)。
    pub connect_timeout_secs: u64,
    pub add_routed_peer_header: bool,
    pub max_body_size: usize,
    pub remote_media_url_policy: u16,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CircuitBreakerSection {
    pub failure_threshold: u32,
    pub success_threshold: u32,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoggingSection {
    pub level: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerEntry {
    pub url: String,
    #[serde(default = "default_max_load")]
    pub max_load: usize,
    #[serde(default = "default_load_penalty")]
    pub load_penalty: usize,
}

fn default_max_load() -> usize {
    20
}

fn default_load_penalty() -> usize {
    0
}

impl AppConfig {
    pub fn effective_dump(&self) -> String {
        serde_yaml::to_string(self).unwrap_or_else(|e| format!("Failed to serialize config: {}", e))
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".to_string(),
            port: 6700,
        }
    }
}

impl Default for CacheSection {
    fn default() -> Self {
        Self {
            threshold: 0.3,
            match_abs_threshold: 8192,
            balance_abs_threshold: 5,
            balance_rel_threshold: 1.25,
            eviction_interval_secs: 60,
            max_tree_size: 1048576,
            daily_cleanup_hour_utc: -1,
        }
    }
}

impl Default for HealthSection {
    fn default() -> Self {
        Self {
            endpoint: "/v1/models".to_string(),
            interval_secs: 10,
            failure_threshold: 3,
            success_threshold: 1,
        }
    }
}

impl Default for ProxySection {
    fn default() -> Self {
        Self {
            max_retries: 1,
            initial_backoff_ms: 100,
            max_backoff_ms: 5000,
            backoff_multiplier: 2.0,
            jitter_factor: 0.25,
            request_timeout_secs: 10000,
            connect_timeout_secs: 2,
            add_routed_peer_header: false,
            max_body_size: 10 * 1024 * 1024,
            remote_media_url_policy: 200,
        }
    }
}

impl Default for CircuitBreakerSection {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            success_threshold: 2,
            timeout_secs: 30,
        }
    }
}

impl Default for LoggingSection {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
        }
    }
}

// Consumer-facing config structs (unchanged from original)
#[derive(Debug, Clone)]
pub struct CacheConfig {
    pub cache_threshold: f32,
    pub match_abs_threshold: usize,
    pub balance_abs_threshold: usize,
    pub balance_rel_threshold: f32,
    pub eviction_interval_secs: u64,
    pub max_tree_size: usize,
    pub daily_cleanup_hour_utc: i32,
}

#[derive(Debug, Clone)]
pub struct ProxyConfig {
    pub max_retries: u32,
    pub initial_backoff_ms: u64,
    pub max_backoff_ms: u64,
    pub backoff_multiplier: f32,
    pub jitter_factor: f32,
    pub request_timeout_secs: u64,
    /// 仅约束「建立连接」耗时(TCP+TLS),与整体 request_timeout 独立。
    /// k8s 死 pod IP=黑洞(SYN 被丢)时,不设则请求挂 ~30s(reqwest 内部连接超时;OS 默认更长
    /// ~130s 但被 reqwest 先截断,实测 30s)。设 2s 快失败,不影响长流式生成(与整体 request_timeout 独立)。
    pub connect_timeout_secs: u64,
    pub add_routed_peer_header: bool,
    pub max_body_size: usize,
    pub remote_media_url_policy: u16,
}

#[derive(Debug, Clone)]
pub struct HealthConfig {
    pub endpoint: String,
    pub interval: Duration,
    pub failure_threshold: u32,
    pub success_threshold: u32,
}

/// Merge `right` onto `left` in place.
///
/// Mappings are merged key by key (recursively); sequences and scalars are
/// replaced wholesale, so a later file listing `workers` replaces the whole
/// list rather than appending to it. A null `right` — what an empty or
/// comment-only file parses to — leaves `left` untouched instead of wiping it.
fn merge_yaml(left: &mut serde_yaml::Value, right: serde_yaml::Value) {
    if right.is_null() {
        return;
    }
    match (left, right) {
        (serde_yaml::Value::Mapping(map_left), serde_yaml::Value::Mapping(map_right)) => {
            for (k, v) in map_right {
                match map_left.entry(k) {
                    serde_yaml::mapping::Entry::Occupied(mut slot) => merge_yaml(slot.get_mut(), v),
                    serde_yaml::mapping::Entry::Vacant(slot) => {
                        slot.insert(v);
                    }
                }
            }
        }
        (l, r) => {
            *l = r; // Override non-mapping values with the newer file's values
        }
    }
}

impl AppConfig {
    /// Load and layer every path in order: later files win, mappings merge
    /// recursively, sequences and scalars replace (see [`merge_yaml`]).
    pub fn load(paths: &[PathBuf]) -> Result<Self, Box<dyn std::error::Error>> {
        let mut merged_raw = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());

        for path in paths {
            let file = File::open(path)
                .map_err(|e| format!("Failed to read config file '{}': {}", path.display(), e))?;
            let val: serde_yaml::Value = serde_yaml::from_reader(file)
                .map_err(|e| format!("Failed to parse config file '{}': {}", path.display(), e))?;
            if !val.is_null() && !val.is_mapping() {
                return Err(format!(
                    "Config file '{}' must contain a YAML mapping at the top level",
                    path.display()
                )
                .into());
            }
            merge_yaml(&mut merged_raw, val);
        }

        // Schema errors are reported against the merged tree, so name every
        // file that fed into it — no single file has the offending line.
        let config: AppConfig = serde_yaml::from_value(merged_raw).map_err(|e| {
            let names: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
            format!("Failed to parse config [{}]: {}", names.join(", "), e)
        })?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), Box<dyn std::error::Error>> {
        if self.workers.is_empty() {
            return Err("At least one worker URL is required".into());
        }
        for entry in &self.workers {
            if !entry.url.starts_with("http://") && !entry.url.starts_with("https://") {
                return Err(format!(
                    "Invalid worker URL '{}': must start with http:// or https://",
                    entry.url
                )
                .into());
            }
            if entry.max_load == 0 {
                return Err(format!("max_load must be > 0 for worker '{}'", entry.url).into());
            }
        }
        if !(0.0..=1.0).contains(&self.cache.threshold) {
            return Err(format!(
                "cache.threshold must be 0.0-1.0, got {}",
                self.cache.threshold
            )
            .into());
        }
        if !(0.0..=1.0).contains(&self.proxy.jitter_factor) {
            return Err(format!(
                "proxy.jitter_factor must be 0.0-1.0, got {}",
                self.proxy.jitter_factor
            )
            .into());
        }
        if self.server.port == 0 {
            return Err("server.port must be > 0".into());
        }
        let policy = self.proxy.remote_media_url_policy;
        if policy != 200 && !matches!(policy, 400..=599) {
            return Err(format!(
                "proxy.remote_media_url_policy must be 200 or a valid 4xx/5xx HTTP status code, got {}",
                policy
            )
            .into());
        }
        if self.cache.daily_cleanup_hour_utc > 23 {
            return Err(format!(
                "cache.daily_cleanup_hour_utc must be -1 (disabled) or 0-23, got {}",
                self.cache.daily_cleanup_hour_utc
            )
            .into());
        }
        Ok(())
    }

    pub fn cache_config(&self) -> CacheConfig {
        CacheConfig {
            cache_threshold: self.cache.threshold,
            match_abs_threshold: self.cache.match_abs_threshold,
            balance_abs_threshold: self.cache.balance_abs_threshold,
            balance_rel_threshold: self.cache.balance_rel_threshold,
            eviction_interval_secs: self.cache.eviction_interval_secs,
            max_tree_size: self.cache.max_tree_size,
            daily_cleanup_hour_utc: self.cache.daily_cleanup_hour_utc,
        }
    }

    pub fn proxy_config(&self) -> ProxyConfig {
        ProxyConfig {
            max_retries: self.proxy.max_retries,
            initial_backoff_ms: self.proxy.initial_backoff_ms,
            max_backoff_ms: self.proxy.max_backoff_ms,
            backoff_multiplier: self.proxy.backoff_multiplier,
            jitter_factor: self.proxy.jitter_factor,
            request_timeout_secs: self.proxy.request_timeout_secs,
            connect_timeout_secs: self.proxy.connect_timeout_secs,
            add_routed_peer_header: self.proxy.add_routed_peer_header,
            max_body_size: self.proxy.max_body_size,
            remote_media_url_policy: self.proxy.remote_media_url_policy,
        }
    }

    pub fn health_config(&self) -> HealthConfig {
        HealthConfig {
            endpoint: self.health.endpoint.clone(),
            interval: Duration::from_secs(self.health.interval_secs),
            failure_threshold: self.health.failure_threshold,
            success_threshold: self.health.success_threshold,
        }
    }

    /// Validates that only the workers field has changed between configs.
    /// Returns Ok(()) if reload is safe, Err with details if not.
    pub fn validate_reload_compatibility(&self, new_config: &AppConfig) -> Result<(), String> {
        let mut errors = Vec::new();

        // Check server config
        if self.server != new_config.server {
            errors.push(format!(
                "server config changed (old: {:?}, new: {:?})",
                self.server, new_config.server
            ));
        }

        // Check cache config
        if self.cache != new_config.cache {
            errors.push(format!(
                "cache config changed (old: {:?}, new: {:?})",
                self.cache, new_config.cache
            ));
        }

        // Check health config
        if self.health != new_config.health {
            errors.push(format!(
                "health config changed (old: {:?}, new: {:?})",
                self.health, new_config.health
            ));
        }

        // Check proxy config
        if self.proxy != new_config.proxy {
            errors.push(format!(
                "proxy config changed (old: {:?}, new: {:?})",
                self.proxy, new_config.proxy
            ));
        }

        // Check circuit breaker config
        if self.circuit_breaker != new_config.circuit_breaker {
            errors.push(format!(
                "circuit_breaker config changed (old: {:?}, new: {:?})",
                self.circuit_breaker, new_config.circuit_breaker
            ));
        }

        // Check logging config
        if self.logging != new_config.logging {
            errors.push(format!(
                "logging config changed (old: {:?}, new: {:?})",
                self.logging, new_config.logging
            ));
        }

        if !errors.is_empty() {
            return Err(format!(
                "Config reload rejected: only worker list changes are supported.\nChanges detected:\n  - {}",
                errors.join("\n  - ")
            ));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_load_minimal_config() {
        let yaml = r#"
workers:
  - url: "http://localhost:8050"
"#;
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(yaml.as_bytes()).unwrap();
        file.flush().unwrap();

        let config = AppConfig::load(&[file.path().to_path_buf()]).unwrap();
        assert_eq!(config.workers.len(), 1);
        assert_eq!(config.workers[0].url, "http://localhost:8050");
        assert_eq!(config.workers[0].max_load, 20);
        assert_eq!(config.server.host, "0.0.0.0");
        assert_eq!(config.server.port, 6700);
        assert_eq!(config.cache.threshold, 0.3);
        assert_eq!(config.health.interval_secs, 10);
        assert_eq!(config.proxy.max_retries, 1);
        assert_eq!(config.circuit_breaker.failure_threshold, 5);
        assert_eq!(config.logging.level, "info");
    }

    #[test]
    fn test_load_full_config() {
        let yaml = r#"
server:
  host: "127.0.0.1"
  port: 9090
workers:
  - url: "http://node1:8050"
    max_load: 10
  - url: "http://node2:8050"
    max_load: 30
cache:
  threshold: 0.5
  balance_abs_threshold: 64
  balance_rel_threshold: 2.0
  eviction_interval_secs: 120
  max_tree_size: 131072
health:
  endpoint: "/healthz"
  interval_secs: 20
  failure_threshold: 5
  success_threshold: 2
proxy:
  max_retries: 5
  initial_backoff_ms: 200
  max_backoff_ms: 10000
  backoff_multiplier: 3.0
  jitter_factor: 0.5
  request_timeout_secs: 600
circuit_breaker:
  failure_threshold: 10
  success_threshold: 3
  timeout_secs: 60
logging:
  level: "debug"
"#;
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(yaml.as_bytes()).unwrap();
        file.flush().unwrap();

        let config = AppConfig::load(&[file.path().to_path_buf()]).unwrap();
        assert_eq!(config.server.host, "127.0.0.1");
        assert_eq!(config.server.port, 9090);
        assert_eq!(config.workers.len(), 2);
        assert_eq!(config.workers[0].max_load, 10);
        assert_eq!(config.workers[1].max_load, 30);
        assert_eq!(config.cache.threshold, 0.5);
        assert_eq!(config.cache.balance_abs_threshold, 64);
        assert_eq!(config.health.endpoint, "/healthz");
        assert_eq!(config.proxy.max_retries, 5);
        assert_eq!(config.proxy.request_timeout_secs, 600);
        assert_eq!(config.circuit_breaker.timeout_secs, 60);
        assert_eq!(config.logging.level, "debug");
    }

    #[test]
    fn test_missing_workers_fails() {
        let yaml = r#"
server:
  port: 8080
"#;
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(yaml.as_bytes()).unwrap();
        file.flush().unwrap();

        let result = AppConfig::load(&[file.path().to_path_buf()]);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("missing field `workers`"));
    }

    #[test]
    fn test_empty_workers_fails() {
        let yaml = r#"
workers: []
"#;
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(yaml.as_bytes()).unwrap();
        file.flush().unwrap();

        let result = AppConfig::load(&[file.path().to_path_buf()]);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("At least one worker"));
    }

    #[test]
    fn test_invalid_worker_url() {
        let yaml = r#"
workers:
  - url: "localhost:8050"
"#;
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(yaml.as_bytes()).unwrap();
        file.flush().unwrap();

        let result = AppConfig::load(&[file.path().to_path_buf()]);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("must start with"));
    }

    #[test]
    fn test_zero_max_load_rejected() {
        let yaml = r#"
workers:
  - url: "http://localhost:8050"
    max_load: 0
"#;
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(yaml.as_bytes()).unwrap();
        file.flush().unwrap();

        let result = AppConfig::load(&[file.path().to_path_buf()]);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("max_load must be > 0"));
    }

    #[test]
    fn test_unknown_field_rejected() {
        let yaml = r#"
workers:
  - url: "http://localhost:8050"
unknown_field: "value"
"#;
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(yaml.as_bytes()).unwrap();
        file.flush().unwrap();

        let result = AppConfig::load(&[file.path().to_path_buf()]);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unknown field"));
    }

    #[test]
    fn test_out_of_range_threshold() {
        let yaml = r#"
workers:
  - url: "http://localhost:8050"
cache:
  threshold: 1.5
"#;
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(yaml.as_bytes()).unwrap();
        file.flush().unwrap();

        let result = AppConfig::load(&[file.path().to_path_buf()]);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("cache.threshold must be 0.0-1.0"));
    }

    #[test]
    fn test_conversion_methods() {
        let yaml = r#"
workers:
  - url: "http://localhost:8050"
cache:
  threshold: 0.4
  balance_abs_threshold: 50
proxy:
  max_retries: 4
  initial_backoff_ms: 150
health:
  endpoint: "/status"
  interval_secs: 15
"#;
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(yaml.as_bytes()).unwrap();
        file.flush().unwrap();

        let config = AppConfig::load(&[file.path().to_path_buf()]).unwrap();

        let cache_config = config.cache_config();
        assert_eq!(cache_config.cache_threshold, 0.4);
        assert_eq!(cache_config.balance_abs_threshold, 50);

        let proxy_config = config.proxy_config();
        assert_eq!(proxy_config.max_retries, 4);
        assert_eq!(proxy_config.initial_backoff_ms, 150);
        assert_eq!(proxy_config.request_timeout_secs, 10000);

        let health_config = config.health_config();
        assert_eq!(health_config.endpoint, "/status");
        assert_eq!(health_config.interval, Duration::from_secs(15));
    }

    #[test]
    fn test_reload_compatibility_workers_only() {
        let yaml1 = r#"
workers:
  - url: "http://localhost:8050"
"#;
        let yaml2 = r#"
workers:
  - url: "http://localhost:8050"
  - url: "http://localhost:8051"
"#;
        let mut f1 = NamedTempFile::new().unwrap();
        f1.write_all(yaml1.as_bytes()).unwrap();
        f1.flush().unwrap();
        let mut f2 = NamedTempFile::new().unwrap();
        f2.write_all(yaml2.as_bytes()).unwrap();
        f2.flush().unwrap();

        let c1 = AppConfig::load(&[f1.path().to_path_buf()]).unwrap();
        let c2 = AppConfig::load(&[f2.path().to_path_buf()]).unwrap();
        assert!(c1.validate_reload_compatibility(&c2).is_ok());
    }

    #[test]
    fn test_reload_compatibility_rejects_cache_change() {
        let yaml1 = r#"
workers:
  - url: "http://localhost:8050"
cache:
  threshold: 0.3
"#;
        let yaml2 = r#"
workers:
  - url: "http://localhost:8050"
cache:
  threshold: 0.5
"#;
        let mut f1 = NamedTempFile::new().unwrap();
        f1.write_all(yaml1.as_bytes()).unwrap();
        f1.flush().unwrap();
        let mut f2 = NamedTempFile::new().unwrap();
        f2.write_all(yaml2.as_bytes()).unwrap();
        f2.flush().unwrap();

        let c1 = AppConfig::load(&[f1.path().to_path_buf()]).unwrap();
        let c2 = AppConfig::load(&[f2.path().to_path_buf()]).unwrap();
        let err = c1.validate_reload_compatibility(&c2).unwrap_err();
        assert!(err.contains("cache config changed"));
    }

    #[test]
    fn test_reload_compatibility_rejects_server_change() {
        let yaml1 = r#"
workers:
  - url: "http://localhost:8050"
server:
  port: 8080
"#;
        let yaml2 = r#"
workers:
  - url: "http://localhost:8050"
server:
  port: 9090
"#;
        let mut f1 = NamedTempFile::new().unwrap();
        f1.write_all(yaml1.as_bytes()).unwrap();
        f1.flush().unwrap();
        let mut f2 = NamedTempFile::new().unwrap();
        f2.write_all(yaml2.as_bytes()).unwrap();
        f2.flush().unwrap();

        let c1 = AppConfig::load(&[f1.path().to_path_buf()]).unwrap();
        let c2 = AppConfig::load(&[f2.path().to_path_buf()]).unwrap();
        let err = c1.validate_reload_compatibility(&c2).unwrap_err();
        assert!(err.contains("server config changed"));
    }

    #[test]
    fn test_remote_media_url_policy_default_200() {
        let yaml = r#"
workers:
  - url: "http://localhost:8050"
"#;
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(yaml.as_bytes()).unwrap();
        file.flush().unwrap();

        let config = AppConfig::load(&[file.path().to_path_buf()]).unwrap();
        assert_eq!(config.proxy.remote_media_url_policy, 200);
    }

    #[test]
    fn test_remote_media_url_policy_400_accepted() {
        let yaml = r#"
workers:
  - url: "http://localhost:8050"
proxy:
  remote_media_url_policy: 400
"#;
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(yaml.as_bytes()).unwrap();
        file.flush().unwrap();

        let config = AppConfig::load(&[file.path().to_path_buf()]).unwrap();
        assert_eq!(config.proxy.remote_media_url_policy, 400);
    }

    #[test]
    fn test_remote_media_url_policy_500_accepted() {
        let yaml = r#"
workers:
  - url: "http://localhost:8050"
proxy:
  remote_media_url_policy: 500
"#;
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(yaml.as_bytes()).unwrap();
        file.flush().unwrap();

        let config = AppConfig::load(&[file.path().to_path_buf()]).unwrap();
        assert_eq!(config.proxy.remote_media_url_policy, 500);
    }

    #[test]
    fn test_remote_media_url_policy_999_rejected() {
        let yaml = r#"
workers:
  - url: "http://localhost:8050"
proxy:
  remote_media_url_policy: 999
"#;
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(yaml.as_bytes()).unwrap();
        file.flush().unwrap();

        let result = AppConfig::load(&[file.path().to_path_buf()]);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("remote_media_url_policy must be 200 or a valid 4xx/5xx"));
    }

    #[test]
    fn test_remote_media_url_policy_300_rejected() {
        let yaml = r#"
workers:
  - url: "http://localhost:8050"
proxy:
  remote_media_url_policy: 300
"#;
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(yaml.as_bytes()).unwrap();
        file.flush().unwrap();

        let result = AppConfig::load(&[file.path().to_path_buf()]);
        assert!(result.is_err());
    }

    // ---- multi-file layering (`--config a --config b`) ----

    #[test]
    fn test_config_flag_is_repeatable() {
        // clap_derive infers ArgAction::Append from the `Vec<PathBuf>` field
        // type, so no explicit `action = ...` is needed. Pin that here: with
        // ArgAction::Set the second -c would overwrite the first.
        let args = CliArgs::parse_from(["cache-aware-router", "-c", "a.yaml", "-c", "b.yaml"]);
        assert_eq!(
            args.config,
            vec![PathBuf::from("a.yaml"), PathBuf::from("b.yaml")]
        );

        let defaulted = CliArgs::parse_from(["cache-aware-router"]);
        assert_eq!(defaulted.config, vec![PathBuf::from("config.yaml")]);
    }

    fn tmp_yaml(contents: &str) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(contents.as_bytes()).unwrap();
        file.flush().unwrap();
        file
    }

    const BASE_YAML: &str = r#"
server:
  host: "127.0.0.1"
  port: 9090
workers:
  - url: "http://node1:8050"
    max_load: 10
  - url: "http://node2:8050"
cache:
  threshold: 0.5
  max_tree_size: 131072
"#;

    #[test]
    fn test_merge_later_file_overrides_scalars() {
        let base = tmp_yaml(BASE_YAML);
        let overlay = tmp_yaml("server:\n  port: 7000\nlogging:\n  level: \"debug\"\n");

        let config =
            AppConfig::load(&[base.path().to_path_buf(), overlay.path().to_path_buf()]).unwrap();
        assert_eq!(config.server.port, 7000);
        // Keys the overlay does not mention survive, at every depth.
        assert_eq!(config.server.host, "127.0.0.1");
        assert_eq!(config.cache.threshold, 0.5);
        assert_eq!(config.cache.max_tree_size, 131072);
        assert_eq!(config.logging.level, "debug");
    }

    #[test]
    fn test_merge_replaces_sequences_wholesale() {
        let base = tmp_yaml(BASE_YAML);
        let overlay = tmp_yaml("workers:\n  - url: \"http://node3:8050\"\n");

        let config =
            AppConfig::load(&[base.path().to_path_buf(), overlay.path().to_path_buf()]).unwrap();
        assert_eq!(config.workers.len(), 1);
        assert_eq!(config.workers[0].url, "http://node3:8050");
    }

    #[test]
    fn test_merge_empty_overlay_is_a_noop() {
        // A ConfigMap-mounted overlay that nothing has written yet must not
        // wipe the base config.
        let base = tmp_yaml(BASE_YAML);
        for overlay_body in ["", "\n", "# nothing here yet\n"] {
            let overlay = tmp_yaml(overlay_body);
            let config =
                AppConfig::load(&[base.path().to_path_buf(), overlay.path().to_path_buf()])
                    .unwrap_or_else(|e| {
                        panic!("empty overlay {:?} broke the merge: {}", overlay_body, e)
                    });
            assert_eq!(config.workers.len(), 2);
            assert_eq!(config.server.port, 9090);
        }
    }

    #[test]
    fn test_merge_three_files_last_wins() {
        let base = tmp_yaml(BASE_YAML);
        let mid = tmp_yaml("server:\n  port: 7000\n");
        let top = tmp_yaml("server:\n  port: 8000\n");

        let config = AppConfig::load(&[
            base.path().to_path_buf(),
            mid.path().to_path_buf(),
            top.path().to_path_buf(),
        ])
        .unwrap();
        assert_eq!(config.server.port, 8000);
    }

    #[test]
    fn test_missing_file_error_names_the_path() {
        let base = tmp_yaml(BASE_YAML);
        let missing = PathBuf::from("/nonexistent/cart-overlay.yaml");

        let err = AppConfig::load(&[base.path().to_path_buf(), missing])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("cart-overlay.yaml"),
            "error lost the path: {}",
            err
        );
    }

    #[test]
    fn test_parse_error_names_the_file() {
        let base = tmp_yaml(BASE_YAML);
        let broken = tmp_yaml("server:\n  port: [unclosed\n");

        let err = AppConfig::load(&[base.path().to_path_buf(), broken.path().to_path_buf()])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(&broken.path().display().to_string()),
            "error lost the path: {}",
            err
        );
    }

    #[test]
    fn test_non_mapping_root_rejected() {
        let base = tmp_yaml(BASE_YAML);
        let scalar = tmp_yaml("just-a-string\n");

        let err = AppConfig::load(&[base.path().to_path_buf(), scalar.path().to_path_buf()])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("must contain a YAML mapping"),
            "unexpected error: {}",
            err
        );
    }
}

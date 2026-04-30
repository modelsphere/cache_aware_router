use clap::Parser;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Minimal CLI: just the config file path and optional validation flag.
#[derive(Parser, Debug)]
#[command(name = "cache-aware-router")]
#[command(about = "Minimal cache-aware reverse proxy for vLLM services")]
pub struct CliArgs {
    /// Path to YAML configuration file
    #[arg(short, long, default_value = "config.yaml")]
    pub config: PathBuf,

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
    pub retry: RetrySection,

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
    pub request_timeout_secs: u64,
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
pub struct RetrySection {
    pub max_retries: u32,
    pub initial_backoff_ms: u64,
    pub max_backoff_ms: u64,
    pub backoff_multiplier: f32,
    pub jitter_factor: f32,
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
}

fn default_max_load() -> usize {
    20
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
            request_timeout_secs: 10000,
        }
    }
}

impl Default for CacheSection {
    fn default() -> Self {
        Self {
            threshold: 0.3,
            match_abs_threshold: 8192,
            balance_abs_threshold: 5,
            balance_rel_threshold: 1.6,
            eviction_interval_secs: 60,
            max_tree_size: 1048576,
        }
    }
}

impl Default for HealthSection {
    fn default() -> Self {
        Self {
            endpoint: "/health".to_string(),
            interval_secs: 10,
            failure_threshold: 3,
            success_threshold: 1,
        }
    }
}

impl Default for RetrySection {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_backoff_ms: 100,
            max_backoff_ms: 5000,
            backoff_multiplier: 2.0,
            jitter_factor: 0.25,
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
}

#[derive(Debug, Clone)]
pub struct RetryConfig {
    pub max_retries: u32,
    pub initial_backoff_ms: u64,
    pub max_backoff_ms: u64,
    pub backoff_multiplier: f32,
    pub jitter_factor: f32,
}

#[derive(Debug, Clone)]
pub struct HealthConfig {
    pub endpoint: String,
    pub interval: Duration,
    pub failure_threshold: u32,
    pub success_threshold: u32,
}

impl AppConfig {
    pub fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let contents = fs::read_to_string(path)
            .map_err(|e| format!("Failed to read config file '{}': {}", path.display(), e))?;
        let config: AppConfig = serde_yaml::from_str(&contents)
            .map_err(|e| format!("Failed to parse config file '{}': {}", path.display(), e))?;
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
                return Err(format!(
                    "max_load must be > 0 for worker '{}'",
                    entry.url
                )
                .into());
            }
        }
        if !(0.0..=1.0).contains(&self.cache.threshold) {
            return Err(format!(
                "cache.threshold must be 0.0-1.0, got {}",
                self.cache.threshold
            )
            .into());
        }
        if !(0.0..=1.0).contains(&self.retry.jitter_factor) {
            return Err(format!(
                "retry.jitter_factor must be 0.0-1.0, got {}",
                self.retry.jitter_factor
            )
            .into());
        }
        if self.server.port == 0 {
            return Err("server.port must be > 0".into());
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
        }
    }

    pub fn retry_config(&self) -> RetryConfig {
        RetryConfig {
            max_retries: self.retry.max_retries,
            initial_backoff_ms: self.retry.initial_backoff_ms,
            max_backoff_ms: self.retry.max_backoff_ms,
            backoff_multiplier: self.retry.backoff_multiplier,
            jitter_factor: self.retry.jitter_factor,
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

    pub fn request_timeout(&self) -> Duration {
        Duration::from_secs(self.server.request_timeout_secs)
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

        // Check retry config
        if self.retry != new_config.retry {
            errors.push(format!(
                "retry config changed (old: {:?}, new: {:?})",
                self.retry, new_config.retry
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

        let config = AppConfig::load(file.path()).unwrap();
        assert_eq!(config.workers.len(), 1);
        assert_eq!(config.workers[0].url, "http://localhost:8050");
        assert_eq!(config.workers[0].max_load, 20);
        assert_eq!(config.server.host, "0.0.0.0");
        assert_eq!(config.server.port, 6700);
        assert_eq!(config.cache.threshold, 0.3);
        assert_eq!(config.health.interval_secs, 10);
        assert_eq!(config.retry.max_retries, 3);
        assert_eq!(config.circuit_breaker.failure_threshold, 5);
        assert_eq!(config.logging.level, "info");
    }

    #[test]
    fn test_load_full_config() {
        let yaml = r#"
server:
  host: "127.0.0.1"
  port: 9090
  request_timeout_secs: 600
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
retry:
  max_retries: 5
  initial_backoff_ms: 200
  max_backoff_ms: 10000
  backoff_multiplier: 3.0
  jitter_factor: 0.5
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

        let config = AppConfig::load(file.path()).unwrap();
        assert_eq!(config.server.host, "127.0.0.1");
        assert_eq!(config.server.port, 9090);
        assert_eq!(config.server.request_timeout_secs, 600);
        assert_eq!(config.workers.len(), 2);
        assert_eq!(config.workers[0].max_load, 10);
        assert_eq!(config.workers[1].max_load, 30);
        assert_eq!(config.cache.threshold, 0.5);
        assert_eq!(config.cache.balance_abs_threshold, 64);
        assert_eq!(config.health.endpoint, "/healthz");
        assert_eq!(config.retry.max_retries, 5);
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

        let result = AppConfig::load(file.path());
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

        let result = AppConfig::load(file.path());
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

        let result = AppConfig::load(file.path());
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

        let result = AppConfig::load(file.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("max_load must be > 0"));
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

        let result = AppConfig::load(file.path());
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

        let result = AppConfig::load(file.path());
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
retry:
  max_retries: 4
  initial_backoff_ms: 150
health:
  endpoint: "/status"
  interval_secs: 15
"#;
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(yaml.as_bytes()).unwrap();
        file.flush().unwrap();

        let config = AppConfig::load(file.path()).unwrap();

        let cache_config = config.cache_config();
        assert_eq!(cache_config.cache_threshold, 0.4);
        assert_eq!(cache_config.balance_abs_threshold, 50);

        let retry_config = config.retry_config();
        assert_eq!(retry_config.max_retries, 4);
        assert_eq!(retry_config.initial_backoff_ms, 150);

        let health_config = config.health_config();
        assert_eq!(health_config.endpoint, "/status");
        assert_eq!(health_config.interval, Duration::from_secs(15));

        let timeout = config.request_timeout();
        assert_eq!(timeout, Duration::from_secs(10000));
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

        let c1 = AppConfig::load(f1.path()).unwrap();
        let c2 = AppConfig::load(f2.path()).unwrap();
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

        let c1 = AppConfig::load(f1.path()).unwrap();
        let c2 = AppConfig::load(f2.path()).unwrap();
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

        let c1 = AppConfig::load(f1.path()).unwrap();
        let c2 = AppConfig::load(f2.path()).unwrap();
        let err = c1.validate_reload_compatibility(&c2).unwrap_err();
        assert!(err.contains("server config changed"));
    }
}

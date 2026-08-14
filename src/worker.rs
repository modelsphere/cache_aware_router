use crate::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
use crate::config::HealthConfig;
use crate::metrics;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use tracing::{debug, info, warn};

/// RAII guard that increments worker load on creation and decrements on drop.
/// Guarantees the load counter stays correct even if an async future is cancelled.
pub struct LoadGuard {
    worker: Arc<Worker>,
}

impl LoadGuard {
    pub fn new(worker: Arc<Worker>) -> Self {
        worker.increment_load();
        Self { worker }
    }

    /// Consume the guard without decrementing, transferring load-tracking
    /// ownership to something else (e.g., a streaming wrapper).
    pub fn disarm(self) -> Arc<Worker> {
        let worker = self.worker.clone();
        std::mem::forget(self);
        worker
    }
}

impl Drop for LoadGuard {
    fn drop(&mut self) {
        self.worker.decrement_load();
    }
}

static HEALTH_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .connect_timeout(std::time::Duration::from_secs(2))
        .build()
        .expect("Failed to create health check HTTP client")
});

pub struct Worker {
    url: String,
    load_counter: AtomicUsize,
    max_load: usize,
    load_penalty: usize,
    healthy: AtomicBool,
    consecutive_failures: AtomicUsize,
    consecutive_successes: AtomicUsize,
    circuit_breaker: CircuitBreaker,
    health_config: HealthConfig,
}

impl fmt::Debug for Worker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Worker")
            .field("url", &self.url)
            .field("healthy", &self.is_healthy())
            .field("load", &self.load())
            .field("cb_state", &self.circuit_breaker.state())
            .finish()
    }
}

impl Worker {
    pub fn new(
        url: String,
        max_load: usize,
        load_penalty: usize,
        health_config: HealthConfig,
        cb_config: CircuitBreakerConfig,
    ) -> Self {
        Self {
            url,
            load_counter: AtomicUsize::new(0),
            max_load,
            load_penalty,
            healthy: AtomicBool::new(true),
            consecutive_failures: AtomicUsize::new(0),
            consecutive_successes: AtomicUsize::new(0),
            circuit_breaker: CircuitBreaker::with_config(cb_config),
            health_config,
        }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Acquire)
    }

    pub fn is_available(&self) -> bool {
        self.is_healthy()
            && self.circuit_breaker.can_execute()
            && self.load_counter.load(Ordering::Relaxed) < self.max_load
    }

    pub fn max_load(&self) -> usize {
        self.max_load
    }

    pub fn load(&self) -> usize {
        self.load_counter.load(Ordering::Relaxed)
    }

    pub fn effective_load(&self) -> usize {
        self.load().saturating_add(self.load_penalty)
    }

    pub fn load_penalty(&self) -> usize {
        self.load_penalty
    }

    pub fn load_display(&self) -> String {
        let load = self.load();
        if self.load_penalty > 0 {
            format!("{}(+{})", load, self.load_penalty)
        } else {
            format!("{}", load)
        }
    }

    pub fn increment_load(&self) {
        self.load_counter.fetch_add(1, Ordering::Relaxed);
        metrics::set_worker_load(&self.url, self.load());
    }

    pub fn decrement_load(&self) {
        // Use fetch_update to prevent underflow
        self.load_counter
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_sub(1)
            })
            .ok();
        metrics::set_worker_load(&self.url, self.load());
    }

    pub fn circuit_breaker(&self) -> &CircuitBreaker {
        &self.circuit_breaker
    }

    pub fn record_outcome(&self, success: bool) {
        self.circuit_breaker.record_outcome(success);
        if !success {
            metrics::record_circuit_breaker_trip(&self.url);
        }
    }

    /// Async health check. Returns true if healthy, false otherwise.
    pub async fn check_health(&self) -> bool {
        let health_url = format!("{}{}", self.url, self.health_config.endpoint);

        match HEALTH_CLIENT.get(&health_url).send().await {
            Ok(response) if response.status().is_success() => {
                let successes = self.consecutive_successes.fetch_add(1, Ordering::Relaxed) + 1;
                self.consecutive_failures.store(0, Ordering::Relaxed);

                if !self.is_healthy() && successes >= self.health_config.success_threshold as usize
                {
                    info!("Worker {} recovered (healthy)", self.url);
                    self.healthy.store(true, Ordering::Release);
                    metrics::record_health_check(&self.url, true);
                }
                true
            }
            Ok(response) => {
                debug!(
                    "Worker {} health check failed: status {}",
                    self.url,
                    response.status()
                );
                self.handle_health_failure();
                false
            }
            Err(e) => {
                debug!("Worker {} health check error: {}", self.url, e);
                self.handle_health_failure();
                false
            }
        }
    }

    fn handle_health_failure(&self) {
        let failures = self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
        self.consecutive_successes.store(0, Ordering::Relaxed);

        if self.is_healthy() && failures >= self.health_config.failure_threshold as usize {
            warn!(
                "Worker {} marked unhealthy after {} consecutive failures",
                self.url, failures
            );
            self.healthy.store(false, Ordering::Release);
            metrics::record_health_check(&self.url, false);
        }
    }
}

/// Background health checker for all workers
pub async fn run_health_checker(workers: Arc<[Arc<Worker>]>, stop: Arc<AtomicBool>) {
    let interval = workers[0].health_config.interval;
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    info!("Health checker started (interval: {:?})", interval);

    loop {
        ticker.tick().await;

        if stop.load(Ordering::Relaxed) {
            info!("Health checker stopping (reload)");
            return;
        }

        // Check all workers concurrently
        let checks: Vec<_> = workers
            .iter()
            .map(|worker| {
                let worker = Arc::clone(worker);
                tokio::spawn(async move { worker.check_health().await })
            })
            .collect();

        for check in checks {
            let _ = check.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HealthConfig;
    use std::time::Duration;

    fn test_worker() -> Arc<Worker> {
        Arc::new(Worker::new(
            "http://test".to_string(),
            10,
            0,
            HealthConfig {
                endpoint: "/v1/models".to_string(),
                interval: Duration::from_secs(10),
                failure_threshold: 3,
                success_threshold: 2,
            },
            CircuitBreakerConfig::default(),
        ))
    }

    #[test]
    fn test_load_guard_increments_and_decrements() {
        let worker = test_worker();
        assert_eq!(worker.load(), 0);

        {
            let _guard = LoadGuard::new(Arc::clone(&worker));
            assert_eq!(worker.load(), 1);
        }

        assert_eq!(worker.load(), 0);
    }

    #[test]
    fn test_load_guard_disarm_prevents_decrement() {
        let worker = test_worker();
        assert_eq!(worker.load(), 0);

        let guard = LoadGuard::new(Arc::clone(&worker));
        assert_eq!(worker.load(), 1);

        let _worker_arc = guard.disarm();
        assert_eq!(worker.load(), 1);

        worker.decrement_load();
        assert_eq!(worker.load(), 0);
    }

    #[test]
    fn test_load_guard_multiple_guards() {
        let worker = test_worker();
        assert_eq!(worker.load(), 0);

        let guard1 = LoadGuard::new(Arc::clone(&worker));
        assert_eq!(worker.load(), 1);

        let guard2 = LoadGuard::new(Arc::clone(&worker));
        assert_eq!(worker.load(), 2);

        drop(guard1);
        assert_eq!(worker.load(), 1);

        drop(guard2);
        assert_eq!(worker.load(), 0);
    }
}

use std::time::Duration;

/// Stub metrics module.
///
/// All functions are no-ops. When you're ready for Prometheus, add the
/// `metrics` + `metrics-exporter-prometheus` crates and fill in the bodies.
/// All call sites are already wired up throughout the codebase.

#[inline(always)]
pub fn record_request_routed(_worker_url: &str) {}

#[inline(always)]
pub fn record_cache_hit(_worker_url: &str) {}

#[inline(always)]
pub fn record_cache_miss() {}

#[inline(always)]
pub fn record_retry(_attempt: u32) {}

#[inline(always)]
pub fn record_circuit_breaker_trip(_worker_url: &str) {}

#[inline(always)]
pub fn set_worker_load(_worker_url: &str, _load: usize) {}

#[inline(always)]
pub fn record_request_duration(_worker_url: &str, _duration: Duration) {}

#[inline(always)]
pub fn record_load_balancing_event() {}

#[inline(always)]
pub fn record_health_check(_worker_url: &str, _healthy: bool) {}

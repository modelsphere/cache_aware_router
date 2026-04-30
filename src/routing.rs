use crate::config::CacheConfig;
use crate::metrics;
use crate::tree::Tree;
use crate::worker::Worker;
use parking_lot::RwLock;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, trace, warn};

pub struct CacheRouter {
    workers: Arc<[Arc<Worker>]>,
    tree: Arc<Tree>,
    config: CacheConfig,
    /// Prevents deadlock between insert and eviction.
    /// Inserts take read lock (concurrent), eviction takes write lock (exclusive).
    eviction_lock: Arc<RwLock<()>>,
}

impl CacheRouter {
    pub fn new(workers: Arc<[Arc<Worker>]>, config: CacheConfig) -> Self {
        let tree = Arc::new(Tree::new());
        let eviction_lock = Arc::new(RwLock::new(()));

        // Register all workers in the tree with empty prefix
        for worker in workers.iter() {
            tree.insert("", worker.url());
        }

        // Spawn background eviction thread if enabled
        if config.eviction_interval_secs > 0 {
            let tree_clone = Arc::clone(&tree);
            let lock_clone = Arc::clone(&eviction_lock);
            let max_size = config.max_tree_size;
            let interval = std::time::Duration::from_secs(config.eviction_interval_secs);

            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(interval);
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

                loop {
                    ticker.tick().await;
                    let start = std::time::Instant::now();
                    {
                        let _guard = lock_clone.write();
                        tree_clone.evict_tenant_by_size(max_size);
                    }
                    unsafe { libc::malloc_trim(0); }
                    info!("Tree eviction pass complete (max_tree_size={}, took {:?})", max_size, start.elapsed());
                }
            });
        }

        Self {
            workers,
            tree,
            config,
            eviction_lock,
        }
    }

    pub fn workers(&self) -> &[Arc<Worker>] {
        &self.workers
    }

    /// Select a worker for the given request text.
    /// Returns worker index, or None if no workers available.
    pub fn select_worker(&self, request_text: &str) -> Option<usize> {
        let start = std::time::Instant::now();

        // Filter to available workers (healthy + circuit breaker open)
        let available: Vec<usize> = self
            .workers
            .iter()
            .enumerate()
            .filter(|(_, w)| w.is_available())
            .map(|(idx, _)| idx)
            .collect();

        if available.is_empty() {
            return None;
        }

        if available.len() == 1 {
            let idx = available[0];
            trace!("Single available worker, routing to {}", self.workers[idx].url());
            return Some(idx);
        }

        // Check if load is imbalanced
        let loads: Vec<usize> = available.iter().map(|&idx| self.workers[idx].load()).collect();
        let max_load = *loads.iter().max().unwrap();
        let min_load = *loads.iter().min().unwrap();

        let abs_diff = max_load.saturating_sub(min_load);
        let rel_ratio = if min_load > 0 {
            max_load as f32 / min_load as f32
        } else {
            f32::INFINITY
        };

        let is_imbalanced = abs_diff >= self.config.balance_abs_threshold
            && rel_ratio >= self.config.balance_rel_threshold;

        if is_imbalanced {
            // Imbalanced mode: route to least-loaded worker
            let min_idx = loads
                .iter()
                .enumerate()
                .min_by_key(|(_, &load)| load)
                .map(|(idx, _)| available[idx])?;

            info!(
                "Route: load_imbalanced → {} load={} | {}/{} {:.2} {:.3}ms",
                self.workers[min_idx].url(), self.workers[min_idx].load(),
                min_load, max_load, rel_ratio,
                start.elapsed().as_secs_f64() * 1000.0
            );
            metrics::record_load_balancing_event();
            metrics::set_load_range(max_load, min_load);

            return Some(min_idx);
        }

        // Balanced mode: use cache-aware routing
        if request_text.is_empty() {
            // No text to match, fall back to least-loaded
            let min_idx = loads
                .iter()
                .enumerate()
                .min_by_key(|(_, &load)| load)
                .map(|(idx, _)| available[idx])?;
            info!(
                "Route: empty_text → {} load={} | {:.3}ms",
                self.workers[min_idx].url(), self.workers[min_idx].load(),
                start.elapsed().as_secs_f64() * 1000.0
            );
            return Some(min_idx);
        }

        // Find best prefix match
        let match_result = self.tree.prefix_match_with_counts(request_text);
        let match_ratio = if match_result.input_char_count > 0 {
            match_result.matched_char_count as f32 / match_result.input_char_count as f32
        } else {
            0.0
        };

        trace!(
            "Prefix match: tenant={}, matched={}/{}, ratio={:.3}",
            match_result.tenant,
            match_result.matched_char_count,
            match_result.input_char_count,
            match_ratio
        );

        let above_ratio = match_ratio >= self.config.cache_threshold;
        let above_absolute = self.config.match_abs_threshold > 0
            && match_result.matched_char_count >= self.config.match_abs_threshold;

        if above_ratio || above_absolute {
            // Good cache hit, route to matched worker
            let matched_worker_idx = self
                .workers
                .iter()
                .position(|w| w.url() == match_result.tenant.as_ref())?;

            // Verify the matched worker is available
            if available.contains(&matched_worker_idx) {
                info!(
                    "Route: cache_hit → {} load={} | matched={}/{} {:.2} {:.3}ms",
                    match_result.tenant, self.workers[matched_worker_idx].load(),
                    match_result.matched_char_count, match_result.input_char_count, match_ratio,
                    start.elapsed().as_secs_f64() * 1000.0
                );
                metrics::record_cache_hit(&match_result.tenant);
                return Some(matched_worker_idx);
            } else {
                debug!(
                    "Matched worker {} unavailable, falling back to least-loaded",
                    match_result.tenant
                );
            }
        }

        // Cache miss or matched worker unavailable: route to least-loaded
        metrics::record_cache_miss();
        let min_idx = loads
            .iter()
            .enumerate()
            .min_by_key(|(_, &load)| load)
            .map(|(idx, _)| available[idx])?;

        info!(
            "Route: cache_miss → {} load={} | matched={}/{} {:.2} {:.3}ms",
            self.workers[min_idx].url(), self.workers[min_idx].load(),
            match_result.matched_char_count, match_result.input_char_count, match_ratio,
            start.elapsed().as_secs_f64() * 1000.0
        );

        Some(min_idx)
    }

    /// Record that a request was routed to the given worker.
    /// This updates the radix tree to track cache state.
    pub fn record_routed(&self, worker_idx: usize, request_text: &str) {
        if request_text.is_empty() {
            return;
        }

        let worker_url = self.workers[worker_idx].url();
        if let Some(_guard) = self.eviction_lock.try_read_for(Duration::from_millis(100)) {
            self.tree.insert(request_text, worker_url);
        } else {
            warn!("Skipping tree insert: eviction lock held >100ms");
        }
        metrics::record_request_routed(worker_url);
    }
}

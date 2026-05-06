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
            let workers_clone = workers.clone();
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
                    let total_load: usize = workers_clone.iter().map(|w| w.load()).sum();
                    info!("Tree eviction pass complete (max_tree_size={}, took {:?}). Active load = {}.", max_size, start.elapsed(), total_load);
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
    pub fn select_worker(&self, request_text: &str, exclude: Option<usize>) -> Option<usize> {
        let start = std::time::Instant::now();

        // Filter to available workers (healthy + circuit breaker open)
        let available: Vec<usize> = self
            .workers
            .iter()
            .enumerate()
            .filter(|(idx, w)| w.is_available() && exclude != Some(*idx))
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

        let loads: Vec<usize> = available.iter().map(|&idx| self.workers[idx].load()).collect();
        let min_load = *loads.iter().min().unwrap();
        let min_idx = loads
            .iter()
            .enumerate()
            .min_by_key(|(_, &load)| load)
            .map(|(idx, _)| available[idx])?;

        if request_text.is_empty() {
            info!(
                "Route: empty_text → {} load={} | {:.3}ms",
                self.workers[min_idx].url(), self.workers[min_idx].load(),
                start.elapsed().as_secs_f64() * 1000.0
            );
            return Some(min_idx);
        }

        // Always run prefix matching first
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
            let matched_worker_idx = self
                .workers
                .iter()
                .position(|w| w.url() == match_result.tenant.as_ref());

            if let Some(matched_worker_idx) = matched_worker_idx.filter(|idx| available.contains(idx)) {
                let matched_load = self.workers[matched_worker_idx].load();

                // Check if the matched worker is itself imbalanced vs. the least-loaded
                let matched_abs_diff = matched_load.saturating_sub(min_load);
                let matched_rel_ratio = if min_load > 0 {
                    matched_load as f32 / min_load as f32
                } else {
                    f32::INFINITY
                };

                let matched_is_overloaded =
                    matched_abs_diff >= self.config.balance_abs_threshold
                        && matched_rel_ratio >= self.config.balance_rel_threshold;

                if matched_is_overloaded {
                    info!(
                        "Route: hit_overloaded → {} load={} | {}/{} {:.2} {:.3}ms",
                        self.workers[min_idx].url(), self.workers[min_idx].load(),
                        min_load, matched_load, matched_rel_ratio,
                        start.elapsed().as_secs_f64() * 1000.0
                    );
                    metrics::record_load_balancing_event();
                    return Some(min_idx);
                }

                info!(
                    "Route: cache_hit → {} load={} | matched={}/{} {:.2} {:.3}ms",
                    match_result.tenant, matched_load,
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

use crate::config::CacheConfig;
use crate::metrics;
use crate::tree::Tree;
use crate::worker::Worker;
use parking_lot::RwLock;
use rand::seq::IndexedRandom;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{debug, info, trace, warn};

pub struct CacheRouter {
    workers: Arc<[Arc<Worker>]>,
    tree: Arc<Tree>,
    config: CacheConfig,
    /// Prevents deadlock between insert and eviction.
    /// Inserts take read lock (concurrent), eviction takes write lock (exclusive).
    eviction_lock: Arc<RwLock<()>>,
    eviction_shutdown: Arc<AtomicBool>,
}

impl CacheRouter {
    pub fn new(workers: Arc<[Arc<Worker>]>, config: CacheConfig) -> Self {
        let tree = Arc::new(Tree::new());
        let eviction_lock = Arc::new(RwLock::new(()));

        // Register all workers in the tree with empty prefix
        for worker in workers.iter() {
            tree.insert("", worker.url());
        }

        let eviction_shutdown = Arc::new(AtomicBool::new(false));

        // Spawn background eviction thread if enabled
        if config.eviction_interval_secs > 0 {
            let tree_clone = Arc::clone(&tree);
            let lock_clone = Arc::clone(&eviction_lock);
            let workers_clone = workers.clone();
            let max_size = config.max_tree_size;
            let interval = std::time::Duration::from_secs(config.eviction_interval_secs);
            let shutdown = Arc::clone(&eviction_shutdown);
            let cleanup_hour = config.daily_cleanup_hour_utc;

            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(interval);
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                let mut last_cleanup_day: Option<u64> = None;

                loop {
                    ticker.tick().await;
                    let is_shutting_down = shutdown.load(Ordering::Relaxed);

                    let do_full_cleanup = is_shutting_down
                        || (cleanup_hour >= 0 && {
                            let secs = SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap()
                                .as_secs();
                            let (today, minute_of_day) = (secs / 86400, (secs % 86400) / 60);
                            let due = minute_of_day >= cleanup_hour as u64 * 60 + 8
                                && last_cleanup_day != Some(today);
                            if due {
                                last_cleanup_day = Some(today);
                            }
                            due
                        });

                    if is_shutting_down {
                        info!("Eviction task shutting down, clearing the tree (reload).");
                    } else if do_full_cleanup {
                        info!("Starting daily tree cleanup (evicting all entries).");
                    }
                    let effective_size = if do_full_cleanup { 0 } else { max_size };
                    let start = std::time::Instant::now();
                    {
                        let _guard = lock_clone.write();
                        tree_clone.evict_tenant_by_size(effective_size);
                    }
                    #[cfg(target_os = "linux")]
                    unsafe {
                        libc::malloc_trim(0);
                    }
                    let total_load: usize = workers_clone.iter().map(|w| w.load()).sum();
                    info!("Tree eviction pass complete (eviction size={}, took {:?}). Active load = {}.", effective_size, start.elapsed(), total_load);

                    if is_shutting_down {
                        info!("Eviction task shutdown completed.");
                        break;
                    }
                }
            });
        }

        Self {
            workers,
            tree,
            config,
            eviction_lock,
            eviction_shutdown,
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
            trace!(
                "Single available worker, routing to {}",
                self.workers[idx].url()
            );
            return Some(idx);
        }

        let loads: Vec<usize> = available
            .iter()
            .map(|&idx| self.workers[idx].effective_load())
            .collect();
        let min_load = *loads.iter().min().unwrap();
        let tied: Vec<usize> = loads
            .iter()
            .enumerate()
            .filter(|(_, &load)| load == min_load)
            .map(|(idx, _)| available[idx])
            .collect();
        let min_idx = match tied.choose(&mut rand::rng()) {
            Some(&idx) => idx,
            None => {
                warn!(
                    "tie-break produced empty candidate set (available={}, loads={:?}) — returning 503",
                    available.len(),
                    loads
                );
                return None;
            }
        };

        if request_text.is_empty() {
            info!(
                "Route: empty_text → {} load={} | {:.3}ms",
                self.workers[min_idx].url(),
                self.workers[min_idx].load_display(),
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

            if let Some(matched_worker_idx) =
                matched_worker_idx.filter(|idx| available.contains(idx))
            {
                let matched_load = self.workers[matched_worker_idx].effective_load();

                // Check if the matched worker is itself imbalanced vs. the least-loaded
                let matched_abs_diff = matched_load.saturating_sub(min_load);
                let matched_rel_ratio = if min_load > 0 {
                    matched_load as f32 / min_load as f32
                } else {
                    f32::INFINITY
                };

                let matched_is_overloaded = matched_abs_diff >= self.config.balance_abs_threshold
                    && matched_rel_ratio >= self.config.balance_rel_threshold;

                if matched_is_overloaded {
                    info!(
                        "Route: hit_overloaded → {} load={} | {}/{} {:.2} {:.3}ms",
                        self.workers[min_idx].url(),
                        self.workers[min_idx].load_display(),
                        min_load,
                        matched_load,
                        matched_rel_ratio,
                        start.elapsed().as_secs_f64() * 1000.0
                    );
                    metrics::record_load_balancing_event();
                    return Some(min_idx);
                }

                info!(
                    "Route: cache_hit → {} load={} | matched={}/{} {:.2} {:.3}ms",
                    match_result.tenant,
                    self.workers[matched_worker_idx].load_display(),
                    match_result.matched_char_count,
                    match_result.input_char_count,
                    match_ratio,
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
            self.workers[min_idx].url(),
            self.workers[min_idx].load_display(),
            match_result.matched_char_count,
            match_result.input_char_count,
            match_ratio,
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
        if let Some(_guard) = self.eviction_lock.try_read_for(Duration::from_secs(1)) {
            self.tree.insert(request_text, worker_url);
        } else {
            warn!("Skipping tree insert: eviction lock held >1s");
        }
        metrics::record_request_routed(worker_url);
    }
}

impl Drop for CacheRouter {
    fn drop(&mut self) {
        self.eviction_shutdown.store(true, Ordering::Relaxed);
    }
}

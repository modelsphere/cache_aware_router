use crate::routing::CacheRouter;
use arc_swap::ArcSwap;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde_json::json;
use std::sync::Arc;
use tikv_jemalloc_ctl::{epoch, stats};
use tracing::info;

pub type DebugState = Arc<ArcSwap<CacheRouter>>;

fn mallctl_read_bool(name: &[u8]) -> Option<bool> {
    let mut val: bool = false;
    let mut len = std::mem::size_of::<bool>();
    let ret = unsafe {
        tikv_jemalloc_sys::mallctl(
            name.as_ptr() as *const _,
            &mut val as *mut _ as *mut _,
            &mut len as *mut _,
            std::ptr::null_mut(),
            0,
        )
    };
    if ret == 0 { Some(val) } else { None }
}

fn mallctl_read_str(name: &[u8]) -> Option<String> {
    let mut ptr: *const libc::c_char = std::ptr::null();
    let mut len = std::mem::size_of::<*const libc::c_char>();
    let ret = unsafe {
        tikv_jemalloc_sys::mallctl(
            name.as_ptr() as *const _,
            &mut ptr as *mut _ as *mut _,
            &mut len as *mut _,
            std::ptr::null_mut(),
            0,
        )
    };
    if ret == 0 && !ptr.is_null() {
        Some(unsafe { std::ffi::CStr::from_ptr(ptr) }.to_string_lossy().into_owned())
    } else if ret == 0 {
        Some(String::new())
    } else {
        None
    }
}

pub async fn heap_stats_handler() -> Response {
    let epoch_result = epoch::advance();

    let allocated = stats::allocated::read();
    let active = stats::active::read();
    let resident = stats::resident::read();
    let mapped = stats::mapped::read();
    let retained = stats::retained::read();
    let metadata = stats::metadata::read();

    let mut errors: Vec<String> = Vec::new();
    if let Err(ref e) = epoch_result {
        errors.push(format!("epoch::advance failed: {}", e));
    }

    macro_rules! stat_or_err {
        ($name:expr, $result:expr) => {
            match $result {
                Ok(v) => v,
                Err(e) => {
                    errors.push(format!("{}: {}", $name, e));
                    0
                }
            }
        };
    }

    let allocated = stat_or_err!("allocated", allocated);
    let active = stat_or_err!("active", active);
    let resident = stat_or_err!("resident", resident);
    let mapped = stat_or_err!("mapped", mapped);
    let retained = stat_or_err!("retained", retained);
    let metadata = stat_or_err!("metadata", metadata);

    let opt_prof = mallctl_read_bool(b"opt.prof\0");
    let prof_active = mallctl_read_bool(b"prof.active\0");
    let opt_malloc_conf = mallctl_read_str(b"opt.malloc_conf\0");
    let config_prof = mallctl_read_bool(b"config.prof\0");

    let mut resp = json!({
        "allocated_bytes": allocated,
        "allocated_mib": format!("{:.2}", allocated as f64 / 1048576.0),
        "active_bytes": active,
        "active_mib": format!("{:.2}", active as f64 / 1048576.0),
        "resident_bytes": resident,
        "resident_mib": format!("{:.2}", resident as f64 / 1048576.0),
        "mapped_bytes": mapped,
        "mapped_mib": format!("{:.2}", mapped as f64 / 1048576.0),
        "retained_bytes": retained,
        "retained_mib": format!("{:.2}", retained as f64 / 1048576.0),
        "metadata_bytes": metadata,
        "metadata_mib": format!("{:.2}", metadata as f64 / 1048576.0),
        "fragmentation_bytes": active.saturating_sub(allocated),
        "fragmentation_ratio": if allocated > 0 {
            format!("{:.4}", active as f64 / allocated as f64)
        } else {
            "N/A".to_string()
        },
        "jemalloc": {
            "config_prof": config_prof,
            "opt_prof": opt_prof,
            "prof_active": prof_active,
            "opt_malloc_conf": opt_malloc_conf,
        },
    });

    if !errors.is_empty() {
        resp.as_object_mut()
            .unwrap()
            .insert("errors".to_string(), json!(errors));
    }

    (StatusCode::OK, Json(resp)).into_response()
}

pub async fn heap_dump_handler() -> Response {
    let config_prof = mallctl_read_bool(b"config.prof\0").unwrap_or(false);
    if !config_prof {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "error": "profiling not compiled into jemalloc",
                "hint": "rebuild: cargo clean && cargo build --release",
            })),
        )
            .into_response();
    }

    let opt_prof = mallctl_read_bool(b"opt.prof\0").unwrap_or(false);
    if !opt_prof {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "error": "profiling compiled in but not enabled at startup (opt.prof=false)",
                "hint": "this binary uses _RJEM_MALLOC_CONF (not MALLOC_CONF). restart with: _RJEM_MALLOC_CONF=\"prof:true,lg_prof_sample:19\" ./cache-aware-router",
                "opt_malloc_conf": mallctl_read_str(b"opt.malloc_conf\0"),
            })),
        )
            .into_response();
    }

    let dump_path = format!(
        "/tmp/jeprof.{}.{}.heap",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    );

    let c_path = match std::ffi::CString::new(dump_path.clone()) {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("invalid path: {}", e)})),
            )
                .into_response();
        }
    };

    let name = b"prof.dump\0";
    let ptr = c_path.as_ptr();
    let ret = unsafe {
        tikv_jemalloc_sys::mallctl(
            name.as_ptr() as *const _,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &ptr as *const _ as *mut _,
            std::mem::size_of::<*const libc::c_char>(),
        )
    };

    if ret != 0 {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "error": "prof.dump mallctl failed",
                "errno": ret,
            })),
        )
            .into_response();
    }

    info!("Heap profile dumped to {}", dump_path);
    (
        StatusCode::OK,
        Json(json!({
            "path": dump_path,
            "hint": "analyze with: jeprof --svg ./target/release/cache-aware-router <path> > heap.svg",
        })),
    )
        .into_response()
}

pub async fn heap_purge_handler() -> Response {
    if let Err(e) = epoch::advance() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("epoch::advance failed: {}", e)})),
        )
            .into_response();
    }

    let before_resident = stats::resident::read().unwrap_or(0);
    let before_retained = stats::retained::read().unwrap_or(0);
    let before_mapped = stats::mapped::read().unwrap_or(0);

    // Purge ALL arenas. jemalloc's MALLCTL_ARENAS_ALL = 4096.
    let narenas_name = b"arenas.narenas\0";
    let mut narenas: libc::c_uint = 0;
    let mut len = std::mem::size_of::<libc::c_uint>();
    unsafe {
        tikv_jemalloc_sys::mallctl(
            narenas_name.as_ptr() as *const _,
            &mut narenas as *mut _ as *mut _,
            &mut len as *mut _,
            std::ptr::null_mut(),
            0,
        );
    }

    // Use the special "all arenas" index (4096) plus per-arena purge as fallback
    let all_purge = std::ffi::CString::new("arena.4096.purge").unwrap();
    let ret = unsafe {
        tikv_jemalloc_sys::mallctl(
            all_purge.as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        )
    };
    if ret != 0 {
        // Fallback: purge each arena individually
        for i in 0..narenas {
            let name = std::ffi::CString::new(format!("arena.{}.purge", i)).unwrap();
            unsafe {
                tikv_jemalloc_sys::mallctl(
                    name.as_ptr(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    0,
                );
            }
        }
    }

    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        libc::malloc_trim(0);
    }

    let _ = epoch::advance();
    let after_resident = stats::resident::read().unwrap_or(0);
    let after_retained = stats::retained::read().unwrap_or(0);
    let after_mapped = stats::mapped::read().unwrap_or(0);

    info!(
        "Heap purge: resident {:.2} → {:.2} MiB",
        before_resident as f64 / 1048576.0,
        after_resident as f64 / 1048576.0
    );

    (
        StatusCode::OK,
        Json(json!({
            "before": {
                "resident_mib": format!("{:.2}", before_resident as f64 / 1048576.0),
                "retained_mib": format!("{:.2}", before_retained as f64 / 1048576.0),
                "mapped_mib": format!("{:.2}", before_mapped as f64 / 1048576.0),
            },
            "after": {
                "resident_mib": format!("{:.2}", after_resident as f64 / 1048576.0),
                "retained_mib": format!("{:.2}", after_retained as f64 / 1048576.0),
                "mapped_mib": format!("{:.2}", after_mapped as f64 / 1048576.0),
            },
            "freed_mib": format!("{:.2}", (before_resident.saturating_sub(after_resident)) as f64 / 1048576.0),
        })),
    )
        .into_response()
}

pub async fn tree_stats_handler(State(router): State<DebugState>) -> Response {
    let router = router.load();
    let tenant_sizes: serde_json::Map<String, serde_json::Value> = router
        .tree()
        .tenant_char_count
        .iter()
        .map(|entry| {
            (
                entry.key().to_string(),
                serde_json::Value::Number((*entry.value()).into()),
            )
        })
        .collect();

    let mut node_count: usize = 0;
    let mut total_tenant_entries: usize = 0;
    let mut max_depth: usize = 0;

    struct StackEntry {
        node: Arc<crate::tree::Node>,
        depth: usize,
    }

    let root = router.tree().root();
    let mut stack = vec![StackEntry {
        node: Arc::clone(root),
        depth: 0,
    }];

    while let Some(StackEntry { node, depth }) = stack.pop() {
        node_count += 1;
        total_tenant_entries += node.tenant_entry_count();
        if depth > max_depth {
            max_depth = depth;
        }
        for child in node.children_vec() {
            stack.push(StackEntry {
                node: child,
                depth: depth + 1,
            });
        }
    }

    (
        StatusCode::OK,
        Json(json!({
            "node_count": node_count,
            "total_tenant_entries": total_tenant_entries,
            "max_depth": max_depth,
            "tenant_char_counts": tenant_sizes,
        })),
    )
        .into_response()
}

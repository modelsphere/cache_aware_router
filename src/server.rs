use crate::config::ProxyConfig;
use crate::proxy;
use crate::routing::CacheRouter;
use arc_swap::ArcSwap;
use axum::body::{Body, Bytes};
use axum::extract::DefaultBodyLimit;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use parking_lot::RwLock;
use serde_json::json;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

struct ModelsCache {
    bytes: Bytes,
    fetched_at: Instant,
}

static MODELS_CACHE: LazyLock<RwLock<Option<ModelsCache>>> = LazyLock::new(|| RwLock::new(None));

#[derive(Clone)]
pub struct AppState {
    pub client: reqwest::Client,
    pub router: Arc<ArcSwap<CacheRouter>>,
    pub proxy_config: ProxyConfig,
}

pub fn build_app(state: AppState) -> Router {
    let max_body_size = state.proxy_config.max_body_size;
    Router::new()
        .route("/v1/chat/completions", post(chat_completions_handler))
        .route("/v1/completions", post(completions_handler))
        .route("/v1/messages", post(messages_handler))
        .route("/v1/models", get(models_handler))
        .route("/health", get(health_handler))
        .route("/workers", get(workers_handler))
        .fallback(fallback_handler)
        .layer(DefaultBodyLimit::max(max_body_size))
        .with_state(state)
}

async fn chat_completions_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let router = state.router.load();
    proxy::proxy_request(
        &state.client,
        &router,
        "/v1/chat/completions",
        &reqwest::Method::POST,
        body,
        &headers,
        &state.proxy_config,
    )
    .await
}

async fn completions_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let router = state.router.load();
    proxy::proxy_request(
        &state.client,
        &router,
        "/v1/completions",
        &reqwest::Method::POST,
        body,
        &headers,
        &state.proxy_config,
    )
    .await
}

async fn messages_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let router = state.router.load();
    proxy::proxy_request(
        &state.client,
        &router,
        "/v1/messages",
        &reqwest::Method::POST,
        body,
        &headers,
        &state.proxy_config,
    )
    .await
}

async fn fallback_handler(State(state): State<AppState>, request: Request) -> Response {
    let path = request.uri().path().to_string();
    let method = request.method().clone();
    let headers = request.headers().clone();

    let body = match axum::body::to_bytes(request.into_body(), usize::MAX).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "Failed to read request body").into_response(),
    };

    let reqwest_method =
        reqwest::Method::from_bytes(method.as_str().as_bytes()).unwrap_or(reqwest::Method::GET);

    let router = state.router.load();
    proxy::proxy_request(
        &state.client,
        &router,
        &path,
        &reqwest_method,
        body,
        &headers,
        &state.proxy_config,
    )
    .await
}

const MODELS_CACHE_TTL: Duration = Duration::from_secs(600);

async fn models_handler(State(state): State<AppState>) -> Response {
    let expired = MODELS_CACHE
        .read()
        .as_ref()
        .is_none_or(|c| c.fetched_at.elapsed() >= MODELS_CACHE_TTL);

    if expired {
        let router = state.router.load();
        let resp = proxy::proxy_request(
            &state.client,
            &router,
            "/v1/models",
            &reqwest::Method::GET,
            Bytes::new(),
            &HeaderMap::new(),
            &state.proxy_config,
        )
        .await;

        if !resp.status().is_success() {
            return resp;
        }

        let (_parts, body) = resp.into_parts();
        match axum::body::to_bytes(body, 8192).await {
            Ok(bytes) if !bytes.is_empty() => {
                *MODELS_CACHE.write() = Some(ModelsCache {
                    bytes,
                    fetched_at: Instant::now(),
                });
            }
            _ => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
        }
    }

    let bytes = MODELS_CACHE
        .read()
        .as_ref()
        .map(|c| c.bytes.clone())
        .unwrap_or_default();
    if bytes.is_empty() {
        tracing::error!("models cache empty after fetch");
        return (StatusCode::SERVICE_UNAVAILABLE, "Service Unavailable").into_response();
    }
    tracing::info!("GET /v1/models -> OK");
    (
        StatusCode::OK,
        [("content-type", "application/json")],
        Body::from(bytes),
    )
        .into_response()
}

async fn health_handler(State(state): State<AppState>) -> impl IntoResponse {
    let router = state.router.load();
    let workers = router.workers();
    let healthy_count = workers.iter().filter(|w| w.is_healthy()).count();
    let total = workers.len();

    if healthy_count > 0 {
        tracing::info!("GET /health -> OK");
        (
            StatusCode::OK,
            Json(json!({
                "status": "healthy",
                "healthy_workers": healthy_count,
                "total_workers": total,
            })),
        )
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "status": "unhealthy",
                "healthy_workers": 0,
                "total_workers": total,
            })),
        )
    }
}

async fn workers_handler(State(state): State<AppState>) -> impl IntoResponse {
    let router = state.router.load();
    let workers: Vec<_> = router
        .workers()
        .iter()
        .map(|w| {
            json!({
                "url": w.url(),
                "healthy": w.is_healthy(),
                "available": w.is_available(),
                "load": w.load(),
                "effective_load": w.effective_load(),
                "load_penalty": w.load_penalty(),
                "max_load": w.max_load(),
                "circuit_breaker_state": format!("{:?}", w.circuit_breaker().state()),
            })
        })
        .collect();

    Json(json!({
        "workers": workers,
        "total": workers.len(),
    }))
}

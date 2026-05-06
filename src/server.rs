use crate::config::ProxyConfig;
use crate::proxy;
use crate::routing::CacheRouter;
use arc_swap::ArcSwap;
use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use serde_json::json;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub client: reqwest::Client,
    pub router: Arc<ArcSwap<CacheRouter>>,
    pub proxy_config: ProxyConfig,
}

pub fn build_app(state: AppState) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(chat_completions_handler))
        .route("/v1/completions", post(completions_handler))
        .route("/v1/messages", post(messages_handler))
        .route("/health", get(health_handler))
        .route("/workers", get(workers_handler))
        .fallback(fallback_handler)
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

async fn fallback_handler(
    State(state): State<AppState>,
    request: Request,
) -> Response {
    let path = request.uri().path().to_string();
    let method = request.method().clone();
    let headers = request.headers().clone();

    let body = match axum::body::to_bytes(request.into_body(), usize::MAX).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "Failed to read request body").into_response(),
    };

    let reqwest_method = reqwest::Method::from_bytes(method.as_str().as_bytes())
        .unwrap_or(reqwest::Method::GET);

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

async fn health_handler(State(state): State<AppState>) -> impl IntoResponse {
    let router = state.router.load();
    let workers = router.workers();
    let healthy_count = workers.iter().filter(|w| w.is_healthy()).count();
    let total = workers.len();

    if healthy_count > 0 {
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

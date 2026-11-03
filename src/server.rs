use crate::anthropic;
use crate::config::ProxyConfig;
use crate::proxy;
use crate::routing::CacheRouter;
use arc_swap::ArcSwap;
use axum::body::{Body, Bytes};
use axum::extract::rejection::BytesRejection;
use axum::extract::DefaultBodyLimit;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use parking_lot::RwLock;
use serde_json::json;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};
use tracing::warn;

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

/// With `proxy.anthropic_messages_adapter` off, `/v1/messages` is proxied as-is to an
/// engine that serves it natively. With it on, the request is translated,
/// routed as a chat request, and the response translated back.
async fn messages_handler(
    State(state): State<AppState>,
    mut headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    if !state.proxy_config.anthropic_messages_adapter {
        let body = match body {
            Ok(b) => b,
            Err(e) => return e.into_response(),
        };
        let router = state.router.load();
        return proxy::proxy_request(
            &state.client,
            &router,
            "/v1/messages",
            &reqwest::Method::POST,
            body,
            &headers,
            &state.proxy_config,
        )
        .await;
    }

    // e.g. 413 from max_body_size: keep it in Anthropic error shape.
    let body = match body {
        Ok(b) => b,
        Err(e) => return anthropic::error_response(e.status(), &e.body_text()),
    };
    // Parse and translate in a scope of their own: the raw body and both parsed
    // trees are freed before forwarding, so only the translated bytes stay
    // alive while the engine queues and prefills (headers arrive with the
    // first token).
    let (translated, model, stop_sequences) = {
        let req: serde_json::Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => {
                warn!("/v1/messages rejected: invalid JSON body: {e}");
                return anthropic::error_response(
                    StatusCode::BAD_REQUEST,
                    &format!("invalid JSON body: {e}"),
                );
            }
        };
        drop(body);
        let translated = match anthropic::request_to_openai(&req) {
            Ok(v) => v,
            Err(e) => {
                warn!("/v1/messages rejected: {e}");
                return anthropic::error_response(StatusCode::BAD_REQUEST, &e);
            }
        };
        let model = req
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap_or_default()
            .to_string();
        let bytes = match serde_json::to_vec(&translated) {
            Ok(b) => Bytes::from(b),
            Err(e) => {
                return anthropic::error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
            }
        };
        (bytes, model, anthropic::stop_sequences(&req))
    };

    // The response body gets parsed here, so it must not come back compressed.
    headers.remove(header::ACCEPT_ENCODING);
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );

    let router = state.router.load();
    let resp = proxy::proxy_request(
        &state.client,
        &router,
        "/v1/chat/completions",
        &reqwest::Method::POST,
        translated,
        &headers,
        &state.proxy_config,
    )
    .await;
    anthropic::translate_response(resp, &model, stop_sequences).await
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::worker::Worker;
    use std::io::Write;

    /// Fake engine: answers every request with a chat completion whose text is
    /// the path it was called on, so the test sees where CART sent the request.
    async fn spawn_engine() -> String {
        let app = Router::new().fallback(|uri: axum::http::Uri| async move {
            Json(json!({
                "id": "x",
                "choices": [{"message": {"content": uri.path()}, "finish_reason": "stop"}],
            }))
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    /// `translate: None` leaves the key out, to exercise the default.
    async fn spawn_cart(engine: &str, translate: Option<bool>) -> String {
        let mut yaml =
            format!("workers:\n  - url: \"{engine}\"\ncache:\n  eviction_interval_secs: 0\n");
        if let Some(t) = translate {
            yaml.push_str(&format!("proxy:\n  anthropic_messages_adapter: {t}\n"));
        }
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(yaml.as_bytes()).unwrap();
        let config = AppConfig::load(&[file.path().to_path_buf()]).unwrap();
        let workers: Arc<[Arc<Worker>]> = config
            .workers
            .iter()
            .map(|w| {
                Arc::new(Worker::new(
                    w.url.clone(),
                    w.max_load,
                    w.load_penalty,
                    config.health_config(),
                    Default::default(),
                ))
            })
            .collect();
        let state = AppState {
            client: reqwest::Client::new(),
            router: Arc::new(ArcSwap::from_pointee(CacheRouter::new(
                workers,
                config.cache_config(),
            ))),
            proxy_config: config.proxy_config(),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, build_app(state)).await.unwrap() });
        format!("http://{addr}")
    }

    async fn post_messages(cart: &str) -> serde_json::Value {
        reqwest::Client::new()
            .post(format!("{cart}/v1/messages"))
            .json(&json!({"model": "m", "max_tokens": 5, "messages": [{"role": "user", "content": "hi"}]}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn test_messages_passthrough_by_default() {
        let cart = spawn_cart(&spawn_engine().await, None).await;
        let resp = post_messages(&cart).await;
        // Engine saw /v1/messages, and its reply came back untouched.
        assert_eq!(resp["choices"][0]["message"]["content"], "/v1/messages");
    }

    #[tokio::test]
    async fn test_messages_translated_when_translation_on() {
        let cart = spawn_cart(&spawn_engine().await, Some(true)).await;
        let resp = post_messages(&cart).await;
        assert_eq!(resp["type"], "message");
        assert_eq!(resp["content"][0]["text"], "/v1/chat/completions");
    }
}

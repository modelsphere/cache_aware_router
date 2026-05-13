use crate::config::ProxyConfig;
use crate::metrics;
use crate::routing::CacheRouter;
use crate::worker::{LoadGuard, Worker};
use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::stream::Stream;
use futures_util::StreamExt;
use pin_project_lite::pin_project;
use rand::Rng;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tracing::{debug, warn};

pin_project! {
    struct LoadTrackingStream<S> {
        #[pin]
        inner: S,
        worker: Option<Arc<Worker>>,
    }

    impl<S> PinnedDrop for LoadTrackingStream<S> {
        fn drop(this: Pin<&mut Self>) {
            if let Some(worker) = this.project().worker.take() {
                worker.decrement_load();
            }
        }
    }
}

impl<S: Stream> Stream for LoadTrackingStream<S> {
    type Item = S::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.project().inner.poll_next(cx)
    }
}

/// HTTP status codes that are safe to retry
fn is_retryable_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::REQUEST_TIMEOUT
            | StatusCode::TOO_MANY_REQUESTS
            | StatusCode::INTERNAL_SERVER_ERROR
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
    )
}

/// Calculate exponential backoff with jitter
fn backoff_delay(config: &ProxyConfig, attempt: u32) -> Duration {
    let base = config.initial_backoff_ms as f32 * config.backoff_multiplier.powi(attempt as i32);
    let capped = (base as u64).min(config.max_backoff_ms);

    let jitter = config.jitter_factor.clamp(0.0, 1.0);
    if jitter > 0.0 {
        let mut rng = rand::rng();
        let jitter_scale: f32 = rng.random_range(-jitter..=jitter);
        let adjusted = (capped as f64 * (1.0 + jitter_scale as f64)).max(0.0) as u64;
        Duration::from_millis(adjusted)
    } else {
        Duration::from_millis(capped)
    }
}

/// Extract request text from the JSON body for cache-aware routing.
/// Returns empty string on parse failure (graceful degradation).
pub fn extract_request_text(body: &[u8], path: &str) -> String {
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(body) else {
        return String::new();
    };

    match path {
        "/v1/chat/completions" => extract_chat_text(&json).unwrap_or_default(),
        "/v1/completions" => extract_completion_text(&json).unwrap_or_default(),
        "/v1/messages" => extract_messages_text(&json).unwrap_or_default(),
        _ => String::new(),
    }
}

fn extract_chat_text(json: &serde_json::Value) -> Option<String> {
    let mut text = String::new();

    if let Some(tools) = json.get("tools") {
        text.push_str(&tools.to_string());
    }

    if let Some(messages) = json.get("messages") {
        text.push_str(&messages.to_string());
    }

    if text.is_empty() { None } else { Some(text) }
}

fn extract_completion_text(json: &serde_json::Value) -> Option<String> {
    json.get("prompt")
        .and_then(|p| p.as_str())
        .map(|s| s.to_string())
}

/// Extract text from Anthropic /v1/messages format.
/// Order: tools → messages (system prompt is typically stable, messages vary).
fn extract_messages_text(json: &serde_json::Value) -> Option<String> {
    let mut text = String::new();

    if let Some(tools) = json.get("tools") {
        text.push_str(&tools.to_string());
    }

    if let Some(messages) = json.get("messages") {
        text.push_str(&messages.to_string());
    }

    if text.is_empty() { None } else { Some(text) }
}

/// Check if the response indicates streaming (SSE)
fn is_streaming_response(headers: &HeaderMap) -> bool {
    headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.contains("text/event-stream"))
        .unwrap_or(false)
}

/// Headers to strip when forwarding
const STRIP_HEADERS: &[&str] = &["host", "content-length", "transfer-encoding"];

/// Build forwarded headers from the original request
fn build_forward_headers(original: &HeaderMap) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in original.iter() {
        let name_str = name.as_str().to_lowercase();
        if STRIP_HEADERS.contains(&name_str.as_str()) {
            continue;
        }
        headers.insert(name.clone(), value.clone());
    }
    headers
}

/// Forward a request to a specific worker, handling streaming.
/// Returns the response and whether it was successful.
async fn forward_to_worker(
    client: &reqwest::Client,
    worker: &Worker,
    path: &str,
    method: &reqwest::Method,
    body: Bytes,
    headers: &HeaderMap,
    timeout: Duration,
) -> Result<Response, StatusCode> {
    let url = format!("{}{}", worker.url(), path);

    let forward_headers = build_forward_headers(headers);

    let response = client
        .request(method.clone(), &url)
        .headers(reqwest_headers(&forward_headers))
        .body(body.clone())
        .timeout(timeout)
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                debug!("Request to {} timed out", url);
                StatusCode::GATEWAY_TIMEOUT
            } else if e.is_connect() {
                debug!("Connection to {} failed: {}", url, e);
                StatusCode::BAD_GATEWAY
            } else {
                debug!("Request to {} failed: {}", url, e);
                StatusCode::BAD_GATEWAY
            }
        })?;

    let status = response.status();
    let resp_headers = response.headers().clone();

    if is_streaming_response(&resp_headers) {
        // Streaming response: pipe the byte stream through
        let stream = response.bytes_stream().map(|result| {
            result.map_err(|e| {
                axum::Error::new(std::io::Error::new(std::io::ErrorKind::Other, e))
            })
        });

        let mut builder = Response::builder().status(status.as_u16());
        for (name, value) in resp_headers.iter() {
            builder = builder.header(name.as_str(), value.as_bytes());
        }

        Ok(builder
            .body(Body::from_stream(stream))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()))
    } else {
        // Non-streaming: read full body
        let resp_body = response.bytes().await.map_err(|e| {
            debug!("Failed to read response body from {}: {}", url, e);
            StatusCode::BAD_GATEWAY
        })?;

        let mut builder = Response::builder().status(status.as_u16());
        for (name, value) in resp_headers.iter() {
            builder = builder.header(name.as_str(), value.as_bytes());
        }

        Ok(builder
            .body(Body::from(resp_body))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()))
    }
}

/// Convert axum HeaderMap to reqwest HeaderMap
fn reqwest_headers(headers: &HeaderMap) -> reqwest::header::HeaderMap {
    let mut out = reqwest::header::HeaderMap::new();
    for (name, value) in headers.iter() {
        if let (Ok(n), Ok(v)) = (
            reqwest::header::HeaderName::from_bytes(name.as_str().as_bytes()),
            reqwest::header::HeaderValue::from_bytes(value.as_bytes()),
        ) {
            out.insert(n, v);
        }
    }
    out
}

/// Main proxy handler with retry logic.
pub async fn proxy_request(
    client: &reqwest::Client,
    router: &CacheRouter,
    path: &str,
    method: &reqwest::Method,
    body: Bytes,
    headers: &HeaderMap,
    proxy_config: &ProxyConfig,
) -> Response {
    let request_text = extract_request_text(&body, path);
    let start = Instant::now();

    let max_attempts = proxy_config.max_retries + 1;
    let mut last_response: Option<Response> = None;
    let mut last_failed: Option<usize> = None;

    for attempt in 0..max_attempts {
        // Select a worker, excluding the one that just failed
        let worker_idx = match router.select_worker(&request_text, last_failed) {
            Some(idx) => idx,
            None => {
                warn!("No available workers for request to {}", path);
                return (StatusCode::SERVICE_UNAVAILABLE, "No available workers").into_response();
            }
        };

        let worker = Arc::clone(&router.workers()[worker_idx]);

        // RAII guard: load is decremented when guard is dropped (including on cancellation)
        let guard = LoadGuard::new(Arc::clone(&worker));

        let result = forward_to_worker(
            client,
            &worker,
            path,
            method,
            body.clone(),
            headers,
            Duration::from_secs(proxy_config.request_timeout_secs),
        )
        .await;

        match result {
            Ok(response) => {
                let status = response.status();
                let success = status.is_success() || status.is_client_error();

                worker.record_outcome(success);

                if is_retryable_status(status) && attempt + 1 < max_attempts {
                    // Guard drops here → load decremented automatically
                    drop(guard);
                    last_failed = Some(worker_idx);
                    metrics::record_retry(attempt);
                    warn!(
                        "{} → {} retryable error status={} attempt={}/{}, retrying",
                        path, worker.url(), status.as_u16(), attempt + 1, max_attempts
                    );

                    let delay = backoff_delay(proxy_config, attempt);
                    tokio::time::sleep(delay).await;

                    last_response = Some(response);
                    continue;
                }

                if !status.is_success() {
                    warn!(
                        "{} → {} status={}",
                        path, worker.url(), status.as_u16()
                    );
                }

                router.record_routed(worker_idx, &request_text);
                metrics::record_request_duration(worker.url(), start.elapsed());

                let mut response = if is_streaming_sse(&response) {
                    // Streaming: transfer load ownership to stream wrapper
                    let worker_arc = guard.disarm();
                    let (parts, body) = response.into_parts();
                    let tracking_stream = LoadTrackingStream {
                        inner: body.into_data_stream(),
                        worker: Some(worker_arc),
                    };
                    Response::from_parts(parts, Body::from_stream(tracking_stream))
                } else {
                    // Non-streaming: guard drops → load decremented automatically
                    response
                };

                if proxy_config.add_routed_peer_header {
                    response.headers_mut().insert(
                        "x-routed-peer",
                        worker.url().parse().unwrap_or_else(|_| "invalid".parse().unwrap()),
                    );
                }

                return response;
            }
            Err(status) => {
                // Guard drops here → load decremented automatically
                worker.record_outcome(false);

                if attempt + 1 < max_attempts {
                    drop(guard);
                    last_failed = Some(worker_idx);
                    metrics::record_retry(attempt);
                    warn!(
                        "{} → {} connection error status={} attempt={}/{}, retrying",
                        path, worker.url(), status.as_u16(), attempt + 1, max_attempts
                    );

                    let delay = backoff_delay(proxy_config, attempt);
                    tokio::time::sleep(delay).await;

                    continue;
                }

                warn!(
                    "{} → {} connection error status={} all {} attempts exhausted",
                    path, worker.url(), status.as_u16(), max_attempts
                );
                return (status, "Backend unavailable").into_response();
            }
        }
    }

    // Exhausted all retries
    warn!("All {} retry attempts exhausted for {}", max_attempts, path);
    last_response.unwrap_or_else(|| {
        (StatusCode::SERVICE_UNAVAILABLE, "All retry attempts exhausted").into_response()
    })
}

/// Check if a response is a streaming SSE response
fn is_streaming_sse(response: &Response) -> bool {
    response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.contains("text/event-stream"))
        .unwrap_or(false)
}

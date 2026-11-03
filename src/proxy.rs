use crate::{
    config::{Config, USER_AGENT},
    upstream::{patch_payload, upstream_path, HEADER_TIMEOUT},
};
use axum::{
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tracing::{info, warn};

const FORWARDED_RESPONSE_HEADERS: [&str; 5] = [
    "content-type",
    "cache-control",
    "content-encoding",
    "retry-after",
    "x-request-id",
];

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub client: reqwest::Client,
}

impl AppState {
    pub fn new(config: Config, client: reqwest::Client) -> Self {
        Self {
            config: Arc::new(config),
            client,
        }
    }
}

pub fn build_router(state: AppState) -> Router {
    let max_body_bytes = state.config.max_body_bytes;
    Router::new()
        .route("/", get(root))
        .route("/v1", get(root))
        .route("/health", get(health))
        .route("/models", get(models))
        .route("/v1/models", get(models))
        .route("/chat/completions", post(proxy_completion))
        .route("/v1/chat/completions", post(proxy_completion))
        .route("/completions", post(proxy_completion))
        .route("/v1/completions", post(proxy_completion))
        .route("/responses", post(proxy_completion))
        .route("/v1/responses", post(proxy_completion))
        .fallback(fallback)
        .layer(DefaultBodyLimit::max(max_body_bytes))
        .layer(middleware::from_fn(cors))
        .with_state(state)
}

async fn root() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "Index-Translate Proxy Ready\n",
    )
        .into_response()
}

async fn health(State(state): State<AppState>) -> Response {
    Json(json!({
        "status": "ok",
        "service": "index-translate",
        "version": env!("CARGO_PKG_VERSION"),
        "upstream": state.config.api_base,
        "model": state.config.model,
    }))
    .into_response()
}

/// Return the live upstream catalogue when possible, otherwise a static list so
/// Immersive Translate can always fetch its model dropdown while offline.
async fn models(State(state): State<AppState>) -> Response {
    let url = state.config.upstream_url("/models", None);
    let fetched = tokio::time::timeout(
        Duration::from_secs(3),
        state
            .client
            .get(&url)
            .header(header::ACCEPT_ENCODING, "identity")
            .send(),
    )
    .await;

    match fetched {
        Ok(Ok(response)) if response.status().is_success() => match response.bytes().await {
            Ok(body) => match serde_json::from_slice::<Value>(&body) {
                Ok(value @ Value::Object(_)) => return Json(value).into_response(),
                Ok(_) => warn!("upstream /models returned non-object JSON; using built-in list"),
                Err(error) => {
                    warn!(%error, "upstream /models returned invalid JSON; using built-in list")
                }
            },
            Err(error) => {
                warn!(%error, "failed to read upstream /models body; using built-in list")
            }
        },
        Ok(Ok(response)) => {
            warn!(status = %response.status(), "upstream /models failed; using built-in list")
        }
        Ok(Err(error)) => {
            warn!(%error, "upstream /models request failed; using built-in list")
        }
        Err(_) => warn!("upstream /models timed out; using built-in list"),
    }

    Json(state.config.fallback_models()).into_response()
}

async fn proxy_completion(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    body: Bytes,
) -> Response {
    let started = Instant::now();
    let Some(path) = upstream_path(uri.path()) else {
        return error_response(
            StatusCode::NOT_FOUND,
            format!("unsupported endpoint: {}", uri.path()),
        );
    };

    let payload = patch_payload(&body);
    let url = state.config.upstream_url(path, uri.query());
    let request = state
        .client
        .post(&url)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT_ENCODING, "identity")
        .header(header::USER_AGENT, USER_AGENT)
        .body(payload);

    let upstream = match tokio::time::timeout(HEADER_TIMEOUT, request.send()).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            warn!(%error, url = %url, "upstream request failed");
            return error_response(
                StatusCode::BAD_GATEWAY,
                format!("upstream request failed: {error}"),
            );
        }
        Err(_) => {
            warn!(url = %url, "upstream request timed out");
            return error_response(StatusCode::GATEWAY_TIMEOUT, "upstream request timed out");
        }
    };

    let status = upstream.status();
    let upstream_headers = upstream.headers();
    let mut builder = Response::builder().status(status);
    for name in FORWARDED_RESPONSE_HEADERS {
        if let Some(value) = upstream_headers.get(name) {
            builder = builder.header(name, value.clone());
        }
    }

    let stream = upstream
        .bytes_stream()
        .map(|chunk| chunk.map_err(std::io::Error::other));
    let response = match builder.body(Body::from_stream(stream)) {
        Ok(response) => response,
        Err(error) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to build response: {error}"),
            );
        }
    };

    if state.config.access_log {
        info!(
            method = %method,
            path = %uri.path(),
            status = status.as_u16(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "proxied request"
        );
    } else if !status.is_success() {
        warn!(
            method = %method,
            path = %uri.path(),
            status = status.as_u16(),
            "upstream returned an error status"
        );
    }

    response
}

async fn fallback(method: Method, uri: Uri) -> Response {
    if method == Method::GET || method == Method::HEAD {
        return root().await;
    }
    error_response(
        StatusCode::NOT_FOUND,
        format!("no route for {} {}", method, uri.path()),
    )
}

/// Minimal CORS layer so browser extensions and web pages can call the proxy.
async fn cors(request: Request, next: Next) -> Response {
    if request.method() == Method::OPTIONS {
        let mut response = StatusCode::NO_CONTENT.into_response();
        add_cors_headers(response.headers_mut());
        return response;
    }

    let mut response = next.run(request).await;
    add_cors_headers(response.headers_mut());
    response
}

fn add_cors_headers(headers: &mut HeaderMap) {
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("*"),
    );
    headers.insert(
        header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static("86400"),
    );
}

fn error_response(status: StatusCode, message: impl Into<String>) -> Response {
    let message = message.into();
    (
        status,
        Json(json!({
            "error": {
                "message": message,
                "type": "proxy_error",
            }
        })),
    )
        .into_response()
}

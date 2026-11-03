use axum::{
    extract::State,
    http::{header, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use index_translate::{
    config::Config,
    proxy::{build_router, AppState},
    upstream::build_client,
};
use serde_json::{json, Value};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};
use tokio::net::TcpListener;

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<Value>>>);

async fn spawn(app: Router) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

async fn mock_upstream(State(captured): State<Captured>, Json(body): Json<Value>) -> Response {
    captured.0.lock().unwrap().push(body.clone());

    if body.get("stream").and_then(Value::as_bool).unwrap_or(false) {
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\" world\"}}]}\n\n",
            "data: [DONE]\n\n"
        );
        return (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "text/event-stream"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            sse,
        )
            .into_response();
    }

    if body.get("fail").and_then(Value::as_bool).unwrap_or(false) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": {"message": "bad upstream request"}})),
        )
            .into_response();
    }

    Json(json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "译文"},
            "finish_reason": "stop"
        }]
    }))
    .into_response()
}

async fn upstream() -> (SocketAddr, Captured) {
    let captured = Captured::default();
    let app = Router::new()
        .route("/v1/chat/completions", post(mock_upstream))
        .with_state(captured.clone());
    (spawn(app).await, captured)
}

async fn proxy(upstream: SocketAddr) -> SocketAddr {
    let config = Config::new(
        format!("http://{upstream}/v1"),
        "Index-Translate-35B-A3B",
        "127.0.0.1",
        18080,
        false,
        64 * 1024 * 1024,
    )
    .unwrap();
    let app = build_router(AppState::new(config, build_client().unwrap()));
    spawn(app).await
}

#[tokio::test]
async fn injects_defaults_and_forwards_completion() {
    let (upstream_addr, captured) = upstream().await;
    let proxy_addr = proxy(upstream_addr).await;

    let response = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/chat/completions"))
        .json(&json!({
            "model": "client-model",
            "messages": [{"role": "user", "content": "你好"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "译文");

    let seen = captured.0.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0]["chat_template_kwargs"]["enable_thinking"], false);
    assert_eq!(seen[0]["temperature"], 0.0);
    assert_eq!(seen[0]["model"], "client-model");
}

#[tokio::test]
async fn streams_sse_without_rebuffering() {
    let (upstream_addr, _captured) = upstream().await;
    let proxy_addr = proxy(upstream_addr).await;

    let response = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/chat/completions"))
        .json(&json!({
            "model": "client-model",
            "messages": [{"role": "user", "content": "你好"}],
            "stream": true
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/event-stream"
    );
    let body = response.text().await.unwrap();
    assert_eq!(
        body,
        concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\" world\"}}]}\n\n",
            "data: [DONE]\n\n"
        )
    );
}

#[tokio::test]
async fn forwards_upstream_error_status_and_body() {
    let (upstream_addr, _captured) = upstream().await;
    let proxy_addr = proxy(upstream_addr).await;

    let response = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/chat/completions"))
        .json(&json!({
            "model": "client-model",
            "messages": [],
            "fail": true
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["message"], "bad upstream request");
}

#[tokio::test]
async fn models_fall_back_when_upstream_is_down() {
    let config = Config::new(
        "http://127.0.0.1:1/v1",
        "Index-Translate-35B-A3B",
        "127.0.0.1",
        18080,
        false,
        64 * 1024 * 1024,
    )
    .unwrap();
    let app = build_router(AppState::new(config, build_client().unwrap()));
    let proxy_addr = spawn(app).await;

    let response = reqwest::get(format!("http://{proxy_addr}/v1/models"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["object"], "list");
    assert!(body["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|model| model["id"] == "Index-Translate-35B-A3B"));
}

#[tokio::test]
async fn answers_cors_preflight_and_rejects_unknown_endpoints() {
    let (upstream_addr, _captured) = upstream().await;
    let proxy_addr = proxy(upstream_addr).await;
    let client = reqwest::Client::new();

    let preflight = client
        .request(
            Method::OPTIONS,
            format!("http://{proxy_addr}/v1/chat/completions"),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(preflight.status(), StatusCode::NO_CONTENT);
    assert_eq!(preflight.headers()["access-control-allow-origin"], "*");

    let unknown = client
        .post(format!("http://{proxy_addr}/v1/embeddings"))
        .json(&json!({"input": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
    assert_eq!(unknown.headers()["access-control-allow-origin"], "*");
}

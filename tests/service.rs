//! The HTTP surface, driven through the router without a socket or a store.

mod common;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use http_body_util::BodyExt;
use hub::app;
use serde_json::Value;
use tower::ServiceExt;

async fn send(request: Request<Body>) -> (StatusCode, axum::http::HeaderMap, Value) {
    let response = app(common::offline_state())
        .oneshot(request)
        .await
        .expect("a response");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("the body")
        .to_bytes();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("a JSON body")
    };
    (status, headers, json)
}

fn get(path: &str) -> Request<Body> {
    Request::get(path).body(Body::empty()).unwrap()
}

#[tokio::test]
async fn liveness_answers_without_any_store() {
    let (status, _, body) = send(get("/healthz")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
    assert_eq!(body["service"], "hub");
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
}

#[tokio::test]
async fn readiness_says_which_store_is_down() {
    let (status, _, body) = send(get("/api/v1/health")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "degraded");
    assert_eq!(body["database"], "down");
    assert_eq!(body["cache"], "down");
}

#[tokio::test]
async fn the_service_describes_itself() {
    let (status, _, body) = send(get("/.well-known/ruchoir-product")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "hub");
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(body["api"], serde_json::json!(["v1"]));
    assert!(body["features"].is_array());
}

#[tokio::test]
async fn the_contract_is_served() {
    let (status, _, body) = send(get("/api/v1/openapi.json")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["paths"]["/healthz"]["get"].is_object());
    assert!(body["paths"]["/.well-known/ruchoir-product"]["get"].is_object());
}

#[tokio::test]
async fn every_response_carries_the_security_headers_and_a_trace() {
    let (_, headers, _) = send(get("/healthz")).await;
    assert_eq!(
        headers[header::CONTENT_SECURITY_POLICY],
        "default-src 'none'; frame-ancestors 'none'"
    );
    assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    assert_eq!(headers[header::REFERRER_POLICY], "no-referrer");
    assert!(headers.contains_key("traceparent"));
}

#[tokio::test]
async fn an_unknown_path_answers_problem_details() {
    let (status, headers, body) = send(get("/api/v1/nothing-here")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(headers[header::CONTENT_TYPE], "application/problem+json");
    assert_eq!(body["code"], "not_found");
}

#[tokio::test]
async fn a_write_sent_from_another_site_is_refused() {
    let request = Request::builder()
        .method(Method::POST)
        .uri("/api/v1/anything")
        .header("sec-fetch-site", "cross-site")
        .header(header::ORIGIN, "https://elsewhere.example")
        .body(Body::empty())
        .unwrap();
    let (status, _, body) = send(request).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "forbidden");
}

/// A fresh directory of this test run, for the manifest files.
fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("hub-manifest-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A state serving the manifest at `path`.
fn with_manifest(path: Option<std::path::PathBuf>) -> hub::AppState {
    let mut state = common::offline_state();
    let mut config = common::config();
    config.manifest = path;
    state.config = std::sync::Arc::new(config);
    state
}

async fn raw(state: hub::AppState, path: &str) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let response = app(state).oneshot(get(path)).await.expect("a response");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, bytes.to_vec())
}

#[tokio::test]
async fn without_a_manifest_the_route_answers_not_found() {
    let (status, _, _) = raw(with_manifest(None), "/.well-known/ruchoir-manifest").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_manifest_is_served_as_written_public_and_cached() {
    let dir = scratch("served");
    let path = dir.join("manifest.json");
    let first = r#"{"version":1,"domain":"example.org","services":{"hub":{"url":"https://hub.example.org"}}}"#;
    std::fs::write(&path, first).unwrap();
    let state = with_manifest(Some(path.clone()));

    let (status, headers, body) = raw(state.clone(), "/.well-known/ruchoir-manifest").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, first.as_bytes());
    assert_eq!(headers[header::CONTENT_TYPE], "application/json");
    assert_eq!(headers[header::CACHE_CONTROL], "public, max-age=300");
    assert_eq!(headers[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");

    // A product added later by the installer is served without a restart.
    let second = r#"{"version":1,"domain":"example.org","services":{"hub":{"url":"https://hub.example.org"},"drive":{"url":"https://drive.example.org"}}}"#;
    std::fs::write(&path, second).unwrap();
    let (status, _, body) = raw(state, "/.well-known/ruchoir-manifest").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, second.as_bytes());
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn an_unreadable_or_broken_manifest_is_a_server_error() {
    let dir = scratch("broken");
    let missing = dir.join("missing.json");
    let (status, _, _) = raw(
        with_manifest(Some(missing)),
        "/.well-known/ruchoir-manifest",
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);

    let broken = dir.join("broken.json");
    std::fs::write(&broken, "{ not json").unwrap();
    let (status, _, _) = raw(with_manifest(Some(broken)), "/.well-known/ruchoir-manifest").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn the_contract_lists_the_manifest() {
    let (_, _, body) = send(get("/api/v1/openapi.json")).await;
    assert!(body["paths"]["/.well-known/ruchoir-manifest"]["get"].is_object());
}

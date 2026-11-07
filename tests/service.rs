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

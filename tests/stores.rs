//! Readiness on real stores.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use hub::app;
use tower::ServiceExt;

#[tokio::test]
async fn readiness_is_ok_when_both_stores_answer() {
    let Some(state) = common::online_state().await else {
        return;
    };
    let response = app(state)
        .oneshot(Request::get("/api/v1/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["status"], "ok");
    assert_eq!(body["database"], "ok");
    assert_eq!(body["cache"], "ok");
}

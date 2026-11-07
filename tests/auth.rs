//! Who may call the hub: Propolis's tokens for the hub, a product's to publish, a person's for what
//! is theirs, refused with their challenge otherwise.

mod common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::routing::get;
use axum::Router;
use hub::auth::{Person, Publisher, PUBLISH};
use hub::AppState;
use jwt_simple::prelude::*;
use ruchoir_auth::{KeySet, Verifier};
use serde_json::{json, Value};
use tower::ServiceExt;
use uuid::Uuid;

const ISSUER: &str = "https://id.example.org";

/// A hub that trusts `key`, and two routes that need a publisher and a person.
fn hub(key: &ES256KeyPair) -> Router {
    let mut jwk: Value = serde_json::from_str(&key.public_key().to_jwk()).unwrap();
    jwk["kid"] = "k1".into();
    let mut state: AppState = common::offline_state();
    let audience = state.config.public_origin.clone();
    state.verifier = Some(Verifier::new(
        ISSUER,
        audience,
        KeySet::fixed(&json!({ "keys": [jwk] })),
    ));
    Router::new()
        .route("/publish", get(|p: Publisher| async move { p.client_id }))
        .route(
            "/mine",
            get(|p: Person| async move { p.account.to_string() }),
        )
        .with_state(state)
}

fn token(key: &ES256KeyPair, audience: &str, sub: &str, scope: &str, person: bool) -> String {
    let mut custom = json!({ "client_id": "drive", "scope": scope });
    if person {
        custom["auth_time"] = 1_700_000_000.into();
    }
    let claims = Claims::with_custom_claims(custom, Duration::from_mins(5))
        .with_issuer(ISSUER)
        .with_audience(audience)
        .with_subject(sub);
    let options = HeaderOptions {
        signature_type: Some("at+jwt".to_owned()),
        ..Default::default()
    };
    key.sign_with_options(claims, &options).unwrap()
}

async fn call(
    router: &Router,
    path: &str,
    token: Option<&str>,
) -> (StatusCode, String, Option<String>) {
    let mut request = Request::builder().uri(path);
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let challenge = response
        .headers()
        .get(header::WWW_AUTHENTICATE)
        .map(|v| v.to_str().unwrap().to_owned());
    let body = http_body_util::BodyExt::collect(response.into_body())
        .await
        .unwrap()
        .to_bytes();
    (
        status,
        String::from_utf8_lossy(&body).into_owned(),
        challenge,
    )
}

#[tokio::test]
async fn a_product_publishes_and_a_person_reaches_their_own() {
    let key = ES256KeyPair::generate().with_key_id("k1");
    let router = hub(&key);
    let hub_origin = common::config().public_origin;
    let account = Uuid::new_v4();

    let service = token(&key, &hub_origin, "drive", PUBLISH, false);
    assert_eq!(
        call(&router, "/publish", Some(&service)).await.0,
        StatusCode::OK
    );
    assert_eq!(call(&router, "/publish", Some(&service)).await.1, "drive");

    let person = token(&key, &hub_origin, &account.to_string(), "", true);
    let (status, body, _) = call(&router, "/mine", Some(&person)).await;
    assert_eq!((status, body), (StatusCode::OK, account.to_string()));
}

#[tokio::test]
async fn anything_else_is_refused_with_its_challenge() {
    let key = ES256KeyPair::generate().with_key_id("k1");
    let router = hub(&key);
    let hub_origin = common::config().public_origin;
    let account = Uuid::new_v4().to_string();
    let cases = [
        // No token, or one for another service.
        ("/mine", None, StatusCode::UNAUTHORIZED, "invalid_token"),
        (
            "/mine",
            Some(token(&key, "https://drive.example.org", &account, "", true)),
            StatusCode::UNAUTHORIZED,
            "invalid_token",
        ),
        // A person never publishes, a service never reads a person's things.
        (
            "/publish",
            Some(token(&key, &hub_origin, &account, PUBLISH, true)),
            StatusCode::FORBIDDEN,
            "insufficient_scope",
        ),
        (
            "/publish",
            Some(token(&key, &hub_origin, "drive", "", false)),
            StatusCode::FORBIDDEN,
            "insufficient_scope",
        ),
        (
            "/mine",
            Some(token(&key, &hub_origin, "drive", PUBLISH, false)),
            StatusCode::FORBIDDEN,
            "insufficient_scope",
        ),
    ];
    for (path, token, status, error) in cases {
        let (got, _, challenge) = call(&router, path, token.as_deref()).await;
        assert_eq!(got, status, "{path}");
        assert!(challenge.unwrap().contains(error), "{path}");
    }
}

#[tokio::test]
async fn without_an_issuer_nothing_authenticates() {
    let router = Router::new()
        .route(
            "/mine",
            get(|p: Person| async move { p.account.to_string() }),
        )
        .with_state(common::offline_state());
    assert_eq!(
        call(&router, "/mine", Some("anything")).await.0,
        StatusCode::UNAUTHORIZED
    );
}

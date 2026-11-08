//! Preferences end to end on the real stores: a person's settings saved and read back, then obeyed
//! by the inbox, by category and by organisation level.

mod common;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use hub::auth::PUBLISH;
use hub::replica::{self, Membership};
use hub::AppState;
use jwt_simple::prelude::*;
use ruchoir_auth::{KeySet, Verifier};
use serde_json::{json, Value};
use tower::ServiceExt;
use uuid::Uuid;

const ISSUER: &str = "https://id.example.org";

struct Hub {
    state: AppState,
    key: ES256KeyPair,
}

async fn hub() -> Option<Hub> {
    let mut state = common::online_state().await?;
    let key = ES256KeyPair::generate().with_key_id("k1");
    let mut jwk: Value = serde_json::from_str(&key.public_key().to_jwk()).unwrap();
    jwk["kid"] = "k1".into();
    let audience = state.config.public_origin.clone();
    state.verifier = Some(Verifier::new(
        ISSUER,
        audience,
        KeySet::fixed(&json!({ "keys": [jwk] })),
    ));
    Some(Hub { state, key })
}

impl Hub {
    fn token(&self, sub: &str, custom: Value) -> String {
        let claims = Claims::with_custom_claims(custom, Duration::from_mins(5))
            .with_issuer(ISSUER)
            .with_audience(&self.state.config.public_origin)
            .with_subject(sub);
        let options = HeaderOptions {
            signature_type: Some("at+jwt".to_owned()),
            ..Default::default()
        };
        self.key.sign_with_options(claims, &options).unwrap()
    }

    fn person(&self, account: Uuid) -> String {
        self.token(
            &account.to_string(),
            json!({ "client_id": "chat", "auth_time": 1_700_000_000 }),
        )
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        token: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {token}"));
        let body = match body {
            Some(body) => {
                request = request.header(header::CONTENT_TYPE, "application/json");
                Body::from(body.to_string())
            }
            None => Body::empty(),
        };
        let response = hub::app(self.state.clone())
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = http_body_util::BodyExt::collect(response.into_body())
            .await
            .unwrap()
            .to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn publish(&self, category: &str, organisation: Uuid, recipients: &[Uuid]) -> i64 {
        let product = self.token("chat", json!({ "client_id": "chat", "scope": PUBLISH }));
        let (status, body) = self
            .call(
                Method::POST,
                "/api/v1/inbox",
                &product,
                Some(json!({
                    "kind": format!("chat.{category}"),
                    "category": category,
                    "organisation": organisation,
                    "object": Uuid::new_v4().to_string(),
                    "link": "https://chat.example.org/c/1",
                    "recipients": recipients,
                })),
            )
            .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        body["entries"].as_i64().unwrap()
    }

    async fn unread(&self, account: Uuid) -> i64 {
        let (_, page) = self
            .call(Method::GET, "/api/v1/me/inbox", &self.person(account), None)
            .await;
        page["unread_count"].as_i64().unwrap()
    }
}

#[tokio::test]
async fn the_inbox_obeys_each_persons_settings_and_levels() {
    let Some(hub) = hub().await else {
        return;
    };
    let (org, pat, sam) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    for account in [pat, sam] {
        replica::apply(
            &hub.state.db,
            &Membership::Joined {
                organisation: org,
                account,
            },
        )
        .await
        .unwrap();
    }

    // Pat turns replies off in the app; saved and read back.
    let (status, _) = hub
        .call(
            Method::PUT,
            "/api/v1/me/notification-preferences",
            &hub.person(pat),
            Some(json!({ "app": { "reply": false } })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, prefs) = hub
        .call(
            Method::GET,
            "/api/v1/me/notification-preferences",
            &hub.person(pat),
            None,
        )
        .await;
    assert_eq!(prefs["app"]["reply"], false);
    assert_eq!(prefs["app"]["mention"], true, "the rest keeps its default");

    assert_eq!(hub.publish("reply", org, &[pat, sam]).await, 1, "Sam alone");
    assert_eq!((hub.unread(pat).await, hub.unread(sam).await), (0, 1));

    // Sam silences the organisation: a mention there does not reach them, a reminder still does.
    let (status, _) = hub
        .call(
            Method::PUT,
            &format!("/api/v1/me/organisations/{org}/notification-level"),
            &hub.person(sam),
            Some(json!({ "level": "none" })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        hub.publish("mention", org, &[pat, sam]).await,
        1,
        "Pat alone"
    );
    assert_eq!(hub.publish("reminder", org, &[sam]).await, 1);
    assert_eq!(hub.unread(sam).await, 2);
}

#[tokio::test]
async fn settings_and_levels_the_hub_cannot_act_on_are_refused() {
    let Some(hub) = hub().await else {
        return;
    };
    let pat = Uuid::new_v4();
    let (status, problem) = hub
        .call(
            Method::PUT,
            "/api/v1/me/notification-preferences",
            &hub.person(pat),
            Some(json!({ "quiet_hours": true, "quiet_from": "25:00" })),
        )
        .await;
    assert_eq!(
        (status, problem["reason"].as_str()),
        (StatusCode::BAD_REQUEST, Some("quiet_hours"))
    );
    let elsewhere = Uuid::new_v4();
    let (status, _) = hub
        .call(
            Method::PUT,
            &format!("/api/v1/me/organisations/{elsewhere}/notification-level"),
            &hub.person(pat),
            Some(json!({ "level": "none" })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "not one of theirs");
}

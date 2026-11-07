//! Server events end to end on the real stores: taken from their emitter once its signature, source
//! and kind hold, stored for each subscriber but the emitter, delivered byte for byte with their
//! signature, and tried again later when a subscriber is down.

mod common;

use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::http::{header, HeaderMap, Method, Request, StatusCode};
use axum::routing::post;
use axum::Router;
use hub::auth::PUBLISH;
use hub::events::{self, EventKeys, Subscriptions};
use hub::AppState;
use jwt_simple::prelude::*;
use ruchoir_auth::{KeySet, Verifier};
use ruchoir_events::signature::{sign, PublicKeys, SigningKey, SIGNATURE_HEADER};
use ruchoir_events::{Event, Kind};
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use serde_json::{json, Value};
use tower::ServiceExt;
use uuid::Uuid;

const ISSUER: &str = "https://id.example.org";

/// Applying subscriptions replaces them all: the tests of this file take turns.
static TURN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A subscriber's endpoint: it keeps every body and signature it is sent.
type Received = Arc<Mutex<Vec<(Vec<u8>, String)>>>;

async fn subscriber() -> (String, Received) {
    let received: Received = Arc::default();
    let kept = received.clone();
    let router = Router::new().route(
        "/events",
        post(move |headers: HeaderMap, body: Bytes| {
            let kept = kept.clone();
            async move {
                let signature = headers[SIGNATURE_HEADER].to_str().unwrap().to_owned();
                kept.lock().unwrap().push((body.to_vec(), signature));
                StatusCode::NO_CONTENT
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await });
    (format!("http://127.0.0.1:{}", address.port()), received)
}

struct Hub {
    state: AppState,
    token_key: ES256KeyPair,
    drive: SigningKey,
    chat: SigningKey,
}

async fn hub() -> Option<Hub> {
    let mut state = common::online_state().await?;
    let token_key = ES256KeyPair::generate().with_key_id("k1");
    let mut jwk: Value = serde_json::from_str(&token_key.public_key().to_jwk()).unwrap();
    jwk["kid"] = "k1".into();
    let audience = state.config.public_origin.clone();
    state.verifier = Some(Verifier::new(
        ISSUER,
        audience,
        KeySet::fixed(&json!({ "keys": [jwk] })),
    ));
    let (drive, _) = SigningKey::generate("drive-events-1").unwrap();
    let (chat, _) = SigningKey::generate("chat-events-1").unwrap();
    let mut keys = PublicKeys::new();
    keys.insert("drive-events-1", "drive", drive.public_key())
        .unwrap();
    keys.insert("chat-events-1", "chat", chat.public_key())
        .unwrap();
    state.event_keys = EventKeys::fixed(keys);
    Some(Hub {
        state,
        token_key,
        drive,
        chat,
    })
}

impl Hub {
    fn token(&self, client: &str) -> String {
        let claims = Claims::with_custom_claims(
            json!({ "client_id": client, "scope": PUBLISH }),
            Duration::from_mins(5),
        )
        .with_issuer(ISSUER)
        .with_audience(&self.state.config.public_origin)
        .with_subject(client);
        let options = HeaderOptions {
            signature_type: Some("at+jwt".to_owned()),
            ..Default::default()
        };
        self.token_key.sign_with_options(claims, &options).unwrap()
    }

    /// `source` emits `kind`, signed with `key`: the body and its signature header.
    fn event(&self, key: &SigningKey, source: &str, kind: &str) -> (Uuid, Vec<u8>, String) {
        let event = Event::new(
            Kind::new(kind).unwrap(),
            1,
            source,
            None,
            json!({ "file": "f1" }),
        );
        let body = serde_json::to_vec(&event).unwrap();
        let header = sign(&body, key).to_header_value();
        (event.id, body, header)
    }

    async fn emit(&self, client: &str, body: &[u8], signature: &str) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(Method::POST)
            .uri("/api/v1/events")
            .header(
                header::AUTHORIZATION,
                format!("Bearer {}", self.token(client)),
            )
            .header(header::CONTENT_TYPE, "application/json")
            .header(SIGNATURE_HEADER, signature)
            .body(Body::from(body.to_vec()))
            .unwrap();
        let response = hub::app(self.state.clone()).oneshot(request).await.unwrap();
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

    async fn deliveries(&self, event: Uuid) -> Vec<String> {
        let rows = self
            .state
            .db
            .query_all_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT subscriber FROM event_deliveries WHERE event_id = $1 ORDER BY subscriber",
                [event.into()],
            ))
            .await
            .unwrap();
        rows.iter()
            .map(|r| r.try_get("", "subscriber").unwrap())
            .collect()
    }

    async fn subscribe(&self, file: &str, origins: &[String]) {
        Subscriptions::parse(file, origins)
            .unwrap()
            .apply(&self.state.db)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn an_event_is_stored_once_for_each_subscriber_but_its_emitter() {
    let Some(hub) = hub().await else {
        return;
    };
    let _turn = TURN.lock().await;
    let origins = vec![
        "https://calendar.example.org".to_owned(),
        "https://chat.example.org".to_owned(),
        "https://drive.example.org".to_owned(),
    ];
    hub.subscribe(
        r#"
[[subscription]]
service = "calendar"
endpoint = "https://calendar.example.org/api/v1/events"
kinds = ["drive.*"]

[[subscription]]
service = "chat"
endpoint = "https://chat.example.org/api/v1/events"
kinds = ["drive.file.deleted", "propolis.*"]

[[subscription]]
service = "drive"
endpoint = "https://drive.example.org/api/v1/events"
kinds = ["drive.*"]
"#,
        &origins,
    )
    .await;
    let (id, body, signature) = hub.event(&hub.drive, "drive", "drive.file.deleted");
    let (status, problem) = hub.emit("drive", &body, &signature).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{problem}");
    assert_eq!(hub.deliveries(id).await, ["calendar", "chat"]);
    // The same event again changes nothing.
    assert_eq!(
        hub.emit("drive", &body, &signature).await.0,
        StatusCode::ACCEPTED
    );
    assert_eq!(hub.deliveries(id).await.len(), 2);
}

#[tokio::test]
async fn an_event_holds_to_its_signature_its_source_and_its_kind() {
    let Some(hub) = hub().await else {
        return;
    };
    let (_, body, signature) = hub.event(&hub.drive, "drive", "drive.file.deleted");
    // Another body under the same signature.
    let mut tampered = body.clone();
    tampered[10] ^= 1;
    let (status, problem) = hub.emit("drive", &tampered, &signature).await;
    assert_eq!(
        (status, problem["reason"].as_str()),
        (StatusCode::BAD_REQUEST, Some("signature"))
    );
    // The chat's event sent with the drive's token.
    let (_, body, signature) = hub.event(&hub.chat, "chat", "chat.message.deleted");
    let (status, problem) = hub.emit("drive", &body, &signature).await;
    assert_eq!(
        (status, problem["reason"].as_str()),
        (StatusCode::BAD_REQUEST, Some("source"))
    );
    // The drive claiming a kind of the chat's.
    let (_, body, signature) = hub.event(&hub.drive, "drive", "chat.message.deleted");
    let (status, problem) = hub.emit("drive", &body, &signature).await;
    assert_eq!(
        (status, problem["reason"].as_str()),
        (StatusCode::BAD_REQUEST, Some("kind_not_yours"))
    );
    // A key signing for another service than the event names.
    let (_, body, signature) = hub.event(&hub.chat, "drive", "drive.file.deleted");
    let (status, _) = hub.emit("drive", &body, &signature).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn delivery_sends_the_exact_bytes_and_tries_again_later() {
    let Some(hub) = hub().await else {
        return;
    };
    let _turn = TURN.lock().await;
    let (up, received) = subscriber().await;
    let down = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port())
    };
    hub.subscribe(
        &format!(
            r#"
[[subscription]]
service = "calendar"
endpoint = "{up}/events"
kinds = ["drive.file.deleted"]

[[subscription]]
service = "chat"
endpoint = "{down}/events"
kinds = ["drive.file.deleted"]
"#
        ),
        &[up.clone(), down.clone()],
    )
    .await;
    let (id, body, signature) = hub.event(&hub.drive, "drive", "drive.file.deleted");
    assert_eq!(
        hub.emit("drive", &body, &signature).await.0,
        StatusCode::ACCEPTED
    );

    let http = ruchoir_auth::http_client().unwrap();
    for _ in 0..50 {
        events::deliver_due(&hub.state.db, &http).await.unwrap();
        if !received.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let got = received.lock().unwrap().clone();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].0, body, "byte for byte");
    assert_eq!(got[0].1, signature, "with the emitter's signature");

    let row = hub
        .state
        .db
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT attempts, delivered_at IS NULL AS pending, next_attempt_at > now() AS later \
             FROM event_deliveries WHERE event_id = $1 AND subscriber = 'chat'",
            [id.into()],
        ))
        .await
        .unwrap()
        .unwrap();
    let attempts: i32 = row.try_get("", "attempts").unwrap();
    let pending: bool = row.try_get("", "pending").unwrap();
    let later: bool = row.try_get("", "later").unwrap();
    assert!(attempts >= 1 && pending && later);
}

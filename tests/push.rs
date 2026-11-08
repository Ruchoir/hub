//! Push and the mail digest on the real stores: browsers subscribe to known push services only, a
//! woken device is told what is unread in its language, and what stays unread is mailed once,
//! through Propolis.

mod common;

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use hub::auth::PUBLISH;
use hub::push::vapid::VapidKeys;
use hub::push::Push;
use hub::AppState;
use jwt_simple::prelude::*;
use p256::pkcs8::EncodePrivateKey;
use ruchoir_auth::{KeySet, Verifier};
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use serde_json::{json, Value};
use tower::ServiceExt;
use uuid::Uuid;

const ISSUER: &str = "https://id.example.org";

struct Hub {
    router: Router,
    state: AppState,
    key: ES256KeyPair,
    audience: String,
}

async fn hub(change: impl FnOnce(&mut AppState)) -> Option<Hub> {
    let mut state = common::online_state().await?;
    change(&mut state);
    let key = ES256KeyPair::generate().with_key_id("k1");
    let mut jwk: Value = serde_json::from_str(&key.public_key().to_jwk()).unwrap();
    jwk["kid"] = "k1".into();
    let audience = state.config.public_origin.clone();
    state.verifier = Some(Verifier::new(
        ISSUER,
        audience.clone(),
        KeySet::fixed(&json!({ "keys": [jwk] })),
    ));
    Some(Hub {
        router: hub::app(state.clone()),
        state,
        key,
        audience,
    })
}

impl Hub {
    fn token(&self, sub: &str, client: &str, scope: &str, person: bool) -> String {
        let mut custom = json!({ "client_id": client, "scope": scope });
        if person {
            custom["auth_time"] = 1_700_000_000.into();
        }
        let claims = Claims::with_custom_claims(custom, Duration::from_mins(5))
            .with_issuer(ISSUER)
            .with_audience(&self.audience)
            .with_subject(sub);
        let options = HeaderOptions {
            signature_type: Some("at+jwt".to_owned()),
            ..Default::default()
        };
        self.key.sign_with_options(claims, &options).unwrap()
    }

    fn product(&self, client: &str) -> String {
        self.token(client, client, PUBLISH, false)
    }

    fn person(&self, account: Uuid) -> String {
        self.token(&account.to_string(), "chat", "", true)
    }

    async fn send(&self, request: Request<Body>) -> (StatusCode, Value) {
        let response = self.router.clone().oneshot(request).await.unwrap();
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
        self.send(request.body(body).unwrap()).await
    }

    /// How many subscriptions `account` holds for `endpoint`.
    async fn holds(&self, account: Uuid, endpoint: &str) -> i64 {
        self.state
            .db
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT count(*) AS n FROM push_subscriptions WHERE endpoint = $1 AND account_id = $2",
                [endpoint.into(), account.into()],
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get::<i64>("", "n")
            .unwrap()
    }
}

fn mention(recipient: Uuid, object: &str) -> Value {
    json!({
        "kind": "chat.mention",
        "category": "mention",
        "object": object,
        "link": format!("https://chat.example.org/m/{object}"),
        "variables": { "actor_name": "Camille", "place": "#général", "excerpt": "On se voit demain ?" },
        "recipients": [recipient],
    })
}

/// A VAPID key, as the installer writes it.
fn vapid() -> VapidKeys {
    let mut scalar = [0u8; 32];
    scalar[31] = 7;
    let key = p256::ecdsa::SigningKey::from_slice(&scalar).unwrap();
    VapidKeys::from_pem(&key.to_pkcs8_pem(Default::default()).unwrap()).unwrap()
}

fn subscription(endpoint: &str) -> Value {
    json!({
        "endpoint": endpoint,
        "keys": {
            "p256dh": "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4",
            "auth": "BTBZMqHH6r4Tts7J_aSIgg"
        }
    })
}

#[tokio::test]
async fn a_browser_subscribes_to_known_push_services_only() {
    let Some(hub) = hub(|state| {
        state.push = Arc::new(Push::new(
            Some(vapid()),
            ruchoir_auth::http_client().unwrap(),
        ))
    })
    .await
    else {
        return;
    };
    let (ada, bo) = (Uuid::new_v4(), Uuid::new_v4());
    let (status, config) = hub
        .call(Method::GET, "/api/v1/push/config", &hub.person(ada), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(config["available"], true);
    assert_eq!(config["public_key"].as_str().unwrap().len(), 87);

    let path = "/api/v1/me/push/subscription";
    for endpoint in [
        "https://127.0.0.1/hook",
        "http://fcm.googleapis.com/x",
        "https://evil.test/x",
    ] {
        let (status, problem) = hub
            .call(
                Method::PUT,
                path,
                &hub.person(ada),
                Some(subscription(endpoint)),
            )
            .await;
        assert_eq!(
            (status, problem["reason"].as_str()),
            (StatusCode::BAD_REQUEST, Some("endpoint")),
            "{endpoint}"
        );
    }
    let endpoint = format!("https://fcm.googleapis.com/fcm/send/{}", Uuid::new_v4());
    let (status, _) = hub
        .call(
            Method::PUT,
            path,
            &hub.person(ada),
            Some(subscription(&endpoint)),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // The same browser, someone else signed in: it is theirs now, not both.
    let (status, _) = hub
        .call(
            Method::PUT,
            path,
            &hub.person(bo),
            Some(subscription(&endpoint)),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        (
            hub.holds(ada, &endpoint).await,
            hub.holds(bo, &endpoint).await
        ),
        (0, 1)
    );
    // Only one's own is forgotten.
    let forget = json!({ "endpoint": endpoint });
    let (status, _) = hub
        .call(Method::DELETE, path, &hub.person(ada), Some(forget.clone()))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(hub.holds(bo, &endpoint).await, 1);
    let (status, _) = hub
        .call(Method::DELETE, path, &hub.person(bo), Some(forget))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(hub.holds(bo, &endpoint).await, 0);
}

#[tokio::test]
async fn without_a_key_or_a_relay_nothing_subscribes() {
    let Some(hub) = hub(|_| {}).await else {
        return;
    };
    let person = hub.person(Uuid::new_v4());
    let (_, config) = hub
        .call(Method::GET, "/api/v1/push/config", &person, None)
        .await;
    assert_eq!(config, json!({ "available": false }));
    let endpoint = "https://fcm.googleapis.com/fcm/send/x";
    let (status, problem) = hub
        .call(
            Method::PUT,
            "/api/v1/me/push/subscription",
            &person,
            Some(subscription(endpoint)),
        )
        .await;
    assert_eq!(
        (status, problem["reason"].as_str()),
        (StatusCode::CONFLICT, Some("push_off"))
    );
    let phone = json!({ "platform": "ios", "token": "ab".repeat(32) });
    let (status, problem) = hub
        .call(Method::PUT, "/api/v1/me/push/devices", &person, Some(phone))
        .await;
    assert_eq!(
        (status, problem["reason"].as_str()),
        (StatusCode::CONFLICT, Some("no_relay"))
    );
}

#[tokio::test]
async fn a_woken_device_is_told_what_is_unread_in_its_language() {
    let Some(hub) = hub(|_| {}).await else {
        return;
    };
    let ada = Uuid::new_v4();
    let (status, _) = hub
        .call(
            Method::POST,
            "/api/v1/inbox",
            &hub.product("chat"),
            Some(mention(ada, "m1")),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let (status, pending) = hub
        .call(
            Method::GET,
            "/api/v1/me/push/pending?locale=en",
            &hub.person(ada),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{pending}");
    assert_eq!(pending["silent"], false);
    let items = pending["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["title"], "#général");
    assert_eq!(
        items[0]["body"],
        "Camille mentioned you\nOn se voit demain ?"
    );
    assert_eq!(items[0]["link"], "https://chat.example.org/m/m1");
    assert_eq!(items[0]["product"], "chat");

    // No language given: the request's, then French.
    let asked = |language: Option<&str>| {
        let mut request = Request::builder()
            .uri("/api/v1/me/push/pending")
            .header(header::AUTHORIZATION, format!("Bearer {}", hub.person(ada)));
        if let Some(language) = language {
            request = request.header(header::ACCEPT_LANGUAGE, language);
        }
        request.body(Body::empty()).unwrap()
    };
    let (_, pending) = hub.send(asked(Some("de-CH,de;q=0.9"))).await;
    assert_eq!(
        pending["items"][0]["body"],
        "Camille hat Sie erwähnt\nOn se voit demain ?"
    );
    let (_, pending) = hub.send(asked(None)).await;
    assert_eq!(
        pending["items"][0]["body"],
        "Camille vous a mentionné\nOn se voit demain ?"
    );

    // Someone else's entries are never drawn; read on another device, nothing is left to say.
    let (_, other) = hub
        .call(
            Method::GET,
            "/api/v1/me/push/pending",
            &hub.person(Uuid::new_v4()),
            None,
        )
        .await;
    assert_eq!(other["items"], json!([]));
    let (status, _) = hub
        .call(
            Method::POST,
            "/api/v1/me/inbox/read",
            &hub.person(ada),
            None,
        )
        .await;
    assert!(status.is_success(), "{status}");
    let (_, pending) = hub
        .call(
            Method::GET,
            "/api/v1/me/push/pending",
            &hub.person(ada),
            None,
        )
        .await;
    assert_eq!(pending["items"], json!([]));
}

/// A stand-in for Propolis: a token for anyone, and every digest it is asked to send kept.
async fn propolis() -> (String, Arc<Mutex<Vec<Value>>>) {
    let sent = Arc::new(Mutex::new(Vec::new()));
    let kept = sent.clone();
    let app = Router::new()
        .route(
            "/oauth/token",
            post(|| async { Json(json!({ "access_token": "service", "token_type": "Bearer" })) }),
        )
        .route(
            "/api/v1/mail/digest",
            post(move |Json(body): Json<Value>| {
                let kept = kept.clone();
                async move {
                    kept.lock().unwrap().push(body);
                    StatusCode::NO_CONTENT
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{address}"), sent)
}

#[tokio::test]
async fn what_stays_unread_is_mailed_once_through_propolis() {
    let (issuer, sent) = propolis().await;
    let key = Ed25519KeyPair::generate();
    let credentials =
        ruchoir_auth::Credentials::from_pem("hub".to_owned(), &key.to_pem(), "hub-auth-1").unwrap();
    let client =
        hub::propolis::Propolis::new(&issuer, credentials, ruchoir_auth::http_client().unwrap());
    let Some(hub) = hub(move |state| state.propolis = Some(client)).await else {
        return;
    };
    let (ada, bo) = (Uuid::new_v4(), Uuid::new_v4());
    for (account, object) in [(ada, "m1"), (ada, "m2"), (bo, "m3")] {
        let (status, _) = hub
            .call(
                Method::POST,
                "/api/v1/inbox",
                &hub.product("chat"),
                Some(mention(account, object)),
            )
            .await;
        assert_eq!(status, StatusCode::ACCEPTED);
    }
    // Bo read theirs in the app: nothing to catch up on.
    hub.call(Method::POST, "/api/v1/me/inbox/read", &hub.person(bo), None)
        .await;
    let to = |account: Uuid| -> Vec<Value> {
        sent.lock()
            .unwrap()
            .iter()
            .filter(|d| d["account"] == json!(account))
            .cloned()
            .collect()
    };

    // Not yet: the digest waits for a quarter of an hour.
    let now = time::OffsetDateTime::now_utc();
    hub::digest::sweep(&hub.state, now).await.unwrap();
    assert!(to(ada).is_empty());

    let later = now + time::Duration::minutes(16);
    hub::digest::sweep(&hub.state, later).await.unwrap();
    let digests = to(ada);
    assert_eq!(digests.len(), 1);
    let digest = &digests[0];
    assert_eq!(digest["total"], 2);
    // The button leads to the newest entry.
    assert_eq!(digest["link"], "https://chat.example.org/m/m2");
    assert_eq!(
        digest["items"][0]["headline"]["fr"],
        "Camille vous a mentionné · #général"
    );
    assert_eq!(
        digest["items"][0]["headline"]["it"],
        "Camille ti ha menzionato · #général"
    );
    assert_eq!(digest["items"][0]["excerpt"], "On se voit demain ?");
    assert!(to(bo).is_empty());

    // Decided once: the next sweep sends nothing more.
    hub::digest::sweep(&hub.state, later + time::Duration::minutes(1))
        .await
        .unwrap();
    assert_eq!(to(ada).len(), 1);
}

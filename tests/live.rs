//! Live delivery end to end, on the real stores and a real socket: a page's ticket, a product's
//! frame reaching it through Valkey, the ticket spent, a foreign page refused, and the socket closing
//! with its Propolis session.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use futures_util::StreamExt;
use hub::auth::PUBLISH;
use hub::{AppState, Config};
use jwt_simple::prelude::*;
use ruchoir_auth::{KeySet, Verifier};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tower::ServiceExt;
use uuid::Uuid;

const ISSUER: &str = "https://id.example.org";
const PAGE: &str = "https://drive.example.org";

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct Live {
    state: AppState,
    address: SocketAddr,
    key: ES256KeyPair,
}

async fn live() -> Option<Live> {
    let (db, valkey) = common::stores().await?;
    let valkey_url = std::env::var("RUCHOIR_TEST_VALKEY_URL").ok()?;
    let config = Config::from_lookup(|name| match name {
        "HUB_PUBLIC_URL" => Some("https://hub.example.org".to_owned()),
        "HUB_PAGE_ORIGINS" => Some(format!("{PAGE}, https://chat.example.org")),
        "HUB_CLIENT_ID" => Some("hub".to_owned()),
        _ => None,
    })
    .unwrap();
    let key = ES256KeyPair::generate().with_key_id("k1");
    let mut jwk: Value = serde_json::from_str(&key.public_key().to_jwk()).unwrap();
    jwk["kid"] = "k1".into();
    let audience = config.public_origin.clone();
    let state = AppState {
        verifier: Some(Verifier::new(
            ISSUER,
            audience,
            KeySet::fixed(&json!({ "keys": [jwk] })),
        )),
        fanout: hub::live::Fanout::start(&valkey_url, valkey.clone())
            .await
            .unwrap(),
        event_keys: hub::events::EventKeys::fixed(ruchoir_events::signature::PublicKeys::new()),
        config: Arc::new(config),
        db,
        valkey,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = hub::app(state.clone()).into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, router).await });
    Some(Live {
        state,
        address,
        key,
    })
}

impl Live {
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

    /// A token the drive exchanged for `account`, in the Propolis session `sid`.
    fn exchanged(&self, account: Uuid, sid: &str) -> String {
        self.token(
            &account.to_string(),
            json!({ "client_id": "drive", "auth_time": 1_700_000_000, "sid": sid, "act": { "sub": "drive" } }),
        )
    }

    fn product(&self, client: &str) -> String {
        self.token(client, json!({ "client_id": client, "scope": PUBLISH }))
    }

    async fn post(&self, path: &str, token: &str, body: Value) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(Method::POST)
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
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

    async fn ticket(&self, account: Uuid, sid: &str) -> String {
        let (status, body) = self
            .post(
                "/api/v1/live/tickets",
                &self.exchanged(account, sid),
                json!({}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["ticket"].as_str().unwrap().to_owned()
    }

    async fn connect(
        &self,
        ticket: &str,
        origin: &str,
    ) -> Result<Socket, tokio_tungstenite::tungstenite::Error> {
        let mut request = format!("ws://{}/api/v1/live/ws?ticket={ticket}", self.address)
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("origin", origin.parse().unwrap());
        tokio_tungstenite::connect_async(request)
            .await
            .map(|(socket, _)| socket)
    }
}

/// The next text frame of a socket within a few seconds; `None` once it closed.
async fn next_frame(socket: &mut Socket) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + StdDuration::from_secs(5);
    loop {
        let message = tokio::time::timeout_at(deadline, socket.next())
            .await
            .ok()??;
        match message.ok()? {
            Message::Text(text) => return serde_json::from_str(&text).ok(),
            Message::Close(_) => return None,
            _ => continue,
        }
    }
}

#[tokio::test]
async fn a_page_opens_its_socket_with_a_ticket_and_hears_its_product() {
    let Some(live) = live().await else {
        return;
    };
    let account = Uuid::new_v4();
    let ticket = live.ticket(account, "s1").await;
    let mut socket = live.connect(&ticket, PAGE).await.expect("the socket opens");

    // Give the subscription a moment, then a product publishes for this person.
    tokio::time::sleep(StdDuration::from_millis(200)).await;
    let frame = json!({ "audience": [account], "frame": { "v": 1, "type": "drive.file.added", "payload": { "name": "a.pdf" } } });
    let (status, _) = live
        .post("/api/v1/live/publish", &live.product("drive"), frame)
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let received = next_frame(&mut socket).await.expect("a frame");
    assert_eq!(received["type"], "drive.file.added");
    assert_eq!(received["payload"]["name"], "a.pdf");
    assert!(
        received.get("audience").is_none(),
        "the audience never reaches a client"
    );

    // A ticket opens one socket.
    assert!(live.connect(&ticket, PAGE).await.is_err());
}

#[tokio::test]
async fn a_product_speaks_for_itself_and_a_foreign_page_is_refused() {
    let Some(live) = live().await else {
        return;
    };
    let account = Uuid::new_v4();
    let foreign = json!({ "audience": [account], "frame": { "v": 1, "type": "chat.message.created", "payload": {} } });
    let (status, problem) = live
        .post("/api/v1/live/publish", &live.product("drive"), foreign)
        .await;
    assert_eq!(
        (status, problem["reason"].as_str()),
        (StatusCode::BAD_REQUEST, Some("frame_type_not_yours"))
    );

    let ticket = live.ticket(account, "s2").await;
    assert!(live
        .connect(&ticket, "https://evil.example.org")
        .await
        .is_err());
    // A person's own token gets no ticket: only a product acting for them does.
    let own = live.token(
        &account.to_string(),
        json!({ "client_id": "app", "auth_time": 1_700_000_000 }),
    );
    let (status, _) = live.post("/api/v1/live/tickets", &own, json!({})).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_socket_closes_with_its_propolis_session() {
    let Some(live) = live().await else {
        return;
    };
    let account = Uuid::new_v4();
    let mut kept = live
        .connect(&live.ticket(account, "kept").await, PAGE)
        .await
        .unwrap();
    let mut ended = live
        .connect(&live.ticket(account, "ended").await, PAGE)
        .await
        .unwrap();
    tokio::time::sleep(StdDuration::from_millis(200)).await;
    live.state.fanout.close_session("ended").await;
    assert_eq!(next_frame(&mut ended).await, None, "closed");

    // The other session's socket still hears.
    let frame =
        json!({ "audience": [account], "frame": { "v": 1, "type": "drive.ping", "payload": {} } });
    live.post("/api/v1/live/publish", &live.product("drive"), frame)
        .await;
    assert_eq!(next_frame(&mut kept).await.unwrap()["type"], "drive.ping");
}

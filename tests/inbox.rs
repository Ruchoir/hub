//! The inbox end to end on the real stores: a product publishes, each person reads and marks what is
//! theirs, by pages.

mod common;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use hub::auth::PUBLISH;
use jwt_simple::prelude::*;
use ruchoir_auth::{KeySet, Verifier};
use serde_json::{json, Value};
use tower::ServiceExt;
use uuid::Uuid;

const ISSUER: &str = "https://id.example.org";

struct Hub {
    router: Router,
    key: ES256KeyPair,
    audience: String,
}

async fn hub() -> Option<Hub> {
    let mut state = common::online_state().await?;
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
        router: hub::app(state),
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
        let response = self
            .router
            .clone()
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
}

fn mention(recipients: &[Uuid], organisation: Uuid, object: &str) -> Value {
    json!({
        "kind": "chat.mention",
        "category": "mention",
        "organisation": organisation,
        "object": object,
        "link": format!("https://chat.example.org/m/{object}"),
        "variables": { "place": "#général", "excerpt": "On se voit demain ?" },
        "recipients": recipients,
    })
}

#[tokio::test]
async fn a_product_publishes_and_each_person_reads_their_own() {
    let Some(hub) = hub().await else {
        return;
    };
    let (pat, sam, organisation) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let chat = hub.product("chat");
    let (status, published) = hub
        .call(
            Method::POST,
            "/api/v1/inbox",
            &chat,
            Some(mention(&[pat, sam, pat], organisation, "m1")),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{published}");
    assert_eq!(published["entries"], 2, "one per recipient");

    let (status, page) = hub
        .call(Method::GET, "/api/v1/me/inbox", &hub.person(pat), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["unread_count"], 1);
    let entry = &page["entries"][0];
    assert_eq!(entry["product"], "chat");
    assert_eq!(entry["kind"], "chat.mention");
    assert_eq!(entry["variables"]["place"], "#général");
    assert_eq!(entry["read_at"], Value::Null);

    // Pat reads it; Sam's stays unread, and Pat cannot touch Sam's.
    let id = entry["id"].as_str().unwrap().to_owned();
    let (status, _) = hub
        .call(
            Method::POST,
            &format!("/api/v1/me/inbox/{id}/read"),
            &hub.person(pat),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = hub
        .call(
            Method::POST,
            &format!("/api/v1/me/inbox/{id}/read"),
            &hub.person(sam),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, page) = hub
        .call(
            Method::GET,
            "/api/v1/me/inbox?unread=true",
            &hub.person(pat),
            None,
        )
        .await;
    assert_eq!(
        (
            page["unread_count"].as_i64(),
            page["entries"].as_array().unwrap().len()
        ),
        (Some(0), 0)
    );
    let (_, page) = hub
        .call(Method::GET, "/api/v1/me/inbox", &hub.person(sam), None)
        .await;
    assert_eq!(page["unread_count"], 1);
}

#[tokio::test]
async fn the_inbox_comes_by_pages_and_clears_by_organisation() {
    let Some(hub) = hub().await else {
        return;
    };
    let (pat, first, second) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let chat = hub.product("chat");
    for (i, organisation) in [first, first, second].into_iter().enumerate() {
        let (status, _) = hub
            .call(
                Method::POST,
                "/api/v1/inbox",
                &chat,
                Some(mention(&[pat], organisation, &format!("p{i}"))),
            )
            .await;
        assert_eq!(status, StatusCode::ACCEPTED);
    }
    let (_, page) = hub
        .call(
            Method::GET,
            "/api/v1/me/inbox?limit=2",
            &hub.person(pat),
            None,
        )
        .await;
    assert_eq!(page["entries"].as_array().unwrap().len(), 2);
    assert_eq!(page["entries"][0]["object"], "p2", "newest first");
    let next = page["next_before"].as_str().unwrap().to_owned();
    let (_, rest) = hub
        .call(
            Method::GET,
            &format!("/api/v1/me/inbox?limit=2&before={next}"),
            &hub.person(pat),
            None,
        )
        .await;
    assert_eq!(rest["entries"].as_array().unwrap().len(), 1);
    assert_eq!(rest["entries"][0]["object"], "p0");
    assert_eq!(rest["next_before"], Value::Null);

    let (status, _) = hub
        .call(
            Method::POST,
            &format!("/api/v1/me/inbox/read?organisation={first}"),
            &hub.person(pat),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, page) = hub
        .call(Method::GET, "/api/v1/me/inbox", &hub.person(pat), None)
        .await;
    assert_eq!(page["unread_count"], 1, "the other organisation's stays");
    let (_, page) = hub
        .call(
            Method::GET,
            &format!("/api/v1/me/inbox?organisation={first}"),
            &hub.person(pat),
            None,
        )
        .await;
    assert_eq!(page["unread_count"], 0);
}

#[tokio::test]
async fn a_product_publishes_under_its_own_name_only() {
    let Some(hub) = hub().await else {
        return;
    };
    let pat = Uuid::new_v4();
    let mut foreign = mention(&[pat], Uuid::new_v4(), "x");
    foreign["kind"] = "drive.shared".into();
    let (status, problem) = hub
        .call(
            Method::POST,
            "/api/v1/inbox",
            &hub.product("chat"),
            Some(foreign),
        )
        .await;
    assert_eq!(
        (status, problem["reason"].as_str()),
        (StatusCode::BAD_REQUEST, Some("kind_not_yours"))
    );
    // A person never publishes.
    let (status, _) = hub
        .call(
            Method::POST,
            "/api/v1/inbox",
            &hub.person(pat),
            Some(mention(&[pat], Uuid::new_v4(), "y")),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

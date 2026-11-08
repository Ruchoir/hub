//! The members' replica and presence on the real stores: the replica read from a stand-in Propolis's
//! change feed, presence counted across connections, chosen, seen by an organisation's members
//! alone, and asked about by a product.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::extract::Query;
use axum::http::{header, Method, Request, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use hub::live::presence;
use hub::replica::{self, Replica};
use hub::AppState;
use jwt_simple::prelude::*;
use ruchoir_auth::{Credentials, KeySet, Verifier};
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use serde_json::{json, Value};
use tower::ServiceExt;
use uuid::Uuid;

const ISSUER: &str = "https://id.example.org";

/// The replica's feed is one cursor for the whole database: the tests of this file take turns.
static TURN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A stand-in Propolis: a token endpoint, and a change feed of `events` after the cursor it is asked.
async fn stand_in(events: Vec<Value>) -> String {
    let events = Arc::new(events);
    let router = Router::new()
        .route(
            "/oauth/token",
            post(|| async { Json(json!({ "access_token": "t", "expires_in": 300 })) }),
        )
        .route(
            "/api/v1/changes",
            get(
                move |Query(query): Query<std::collections::HashMap<String, String>>| {
                    let events = events.clone();
                    async move {
                        let after: i64 =
                            query.get("after").and_then(|a| a.parse().ok()).unwrap_or(0);
                        let changes: Vec<Value> = events
                            .iter()
                            .enumerate()
                            .map(|(i, e)| json!({ "seq": 1_000_000 + i as i64, "event": e }))
                            .filter(|c| c["seq"].as_i64().unwrap() > after)
                            .collect();
                        let next = changes.last().map_or(after, |c| c["seq"].as_i64().unwrap());
                        Json(json!({ "changes": changes, "next": next }))
                    }
                },
            ),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await });
    format!("http://{address}")
}

fn event(kind: &str, organisation: Uuid, payload: Value) -> Value {
    json!({ "id": Uuid::new_v4(), "kind": kind, "version": 1, "occurred_at": "2026-11-08T00:00:00Z",
            "source": "propolis", "organisation": organisation, "payload": payload })
}

async fn members(state: &AppState, organisation: Uuid) -> Vec<Uuid> {
    let rows = state
        .db
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT account_id FROM organisation_members WHERE organisation_id = $1 ORDER BY account_id",
            [organisation.into()],
        ))
        .await
        .unwrap();
    rows.iter()
        .map(|r| r.try_get("", "account_id").unwrap())
        .collect()
}

#[tokio::test]
async fn the_replica_follows_propolis_change_feed() {
    let Some(state) = common::online_state().await else {
        return;
    };
    let _turn = TURN.lock().await;
    // Start after whatever another run left: this feed's cursor goes from a million.
    state
        .db
        .execute_unprepared("DELETE FROM replica_cursors")
        .await
        .unwrap();
    let (org, owner, pat, sam) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    let member = |id: Uuid| json!({ "actor": null, "target": { "kind": "member", "id": id } });
    let issuer = stand_in(vec![
        event(
            "propolis.organisation.created",
            org,
            json!({ "actor": owner }),
        ),
        event("propolis.member.joined", org, member(pat)),
        event("propolis.member.joined", org, member(sam)),
        event("propolis.member.standing_changed", org, member(sam)),
        event("propolis.member.left", org, member(pat)),
    ])
    .await;
    let credentials = Credentials::new("hub", Ed25519KeyPair::generate(), "hub-auth");
    let replica = Replica::new(hub::propolis::Propolis::new(
        &issuer,
        credentials,
        ruchoir_auth::http_client().unwrap(),
    ));
    let applied = replica.sync(&state.db).await.unwrap();
    assert_eq!(applied, 4, "the standing change is not membership");
    let mut expected = vec![owner, sam];
    expected.sort();
    assert_eq!(members(&state, org).await, expected);
    // Read again: nothing new, nothing changes.
    assert_eq!(replica.sync(&state.db).await.unwrap(), 0);
    let mut together = replica::co_members(&state.db, owner).await.unwrap();
    together.sort();
    assert_eq!(together, expected);
}

fn person_token(key: &ES256KeyPair, audience: &str, account: Uuid) -> String {
    let claims = Claims::with_custom_claims(
        json!({ "client_id": "chat", "auth_time": 1_700_000_000 }),
        Duration::from_mins(5),
    )
    .with_issuer(ISSUER)
    .with_audience(audience)
    .with_subject(account.to_string());
    let options = HeaderOptions {
        signature_type: Some("at+jwt".to_owned()),
        ..Default::default()
    };
    key.sign_with_options(claims, &options).unwrap()
}

async fn call(
    state: &AppState,
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
    let response = hub::app(state.clone())
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

#[tokio::test]
async fn presence_is_counted_chosen_and_seen_by_the_members_alone() {
    let Some(mut state) = common::online_state().await else {
        return;
    };
    let key = ES256KeyPair::generate().with_key_id("k1");
    let mut jwk: Value = serde_json::from_str(&key.public_key().to_jwk()).unwrap();
    jwk["kid"] = "k1".into();
    let audience = state.config.public_origin.clone();
    state.verifier = Some(Verifier::new(
        ISSUER,
        audience.clone(),
        KeySet::fixed(&json!({ "keys": [jwk] })),
    ));

    let (org, pat, sam, stranger) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    for account in [pat, sam] {
        replica::apply(
            &state.db,
            &replica::Membership::Joined {
                organisation: org,
                account,
            },
        )
        .await
        .unwrap();
    }

    // Two connections: Pat stays online until the last one goes.
    presence::arrived(&state, pat).await;
    presence::arrived(&state, pat).await;
    presence::departed(&state, pat).await;
    assert!(presence::is_online(&state.valkey, pat).await);
    let path = format!("/api/v1/organisations/{org}/presence");
    let (status, list) = call(
        &state,
        Method::GET,
        &path,
        &person_token(&key, &audience, sam),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let of = |list: &Value, account: Uuid| {
        list.as_array()
            .unwrap()
            .iter()
            .find(|m| m["account"] == account.to_string().as_str())
            .map(|m| m["presence"].as_str().unwrap().to_owned())
    };
    assert_eq!(of(&list, pat).as_deref(), Some("active"));
    assert_eq!(of(&list, sam).as_deref(), Some("offline"));

    // Pat chooses do-not-disturb, then invisible: others see it, then offline; Pat sees their own.
    let pat_token = person_token(&key, &audience, pat);
    for (choice, seen) in [("dnd", "dnd"), ("invisible", "offline")] {
        let (status, _) = call(
            &state,
            Method::PUT,
            "/api/v1/me/presence",
            &pat_token,
            Some(json!({ "choice": choice })),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (_, list) = call(
            &state,
            Method::GET,
            &path,
            &person_token(&key, &audience, sam),
            None,
        )
        .await;
        assert_eq!(of(&list, pat).as_deref(), Some(seen));
    }
    let (_, mine) = call(&state, Method::GET, "/api/v1/me/presence", &pat_token, None).await;
    assert_eq!(
        (mine["choice"].as_str(), mine["presence"].as_str()),
        (Some("invisible"), Some("invisible"))
    );
    let (status, _) = call(
        &state,
        Method::PUT,
        "/api/v1/me/presence",
        &pat_token,
        Some(json!({ "choice": "busy" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // The last connection goes: offline.
    presence::departed(&state, pat).await;
    assert!(!presence::is_online(&state.valkey, pat).await);

    // Someone outside the organisation sees nothing of it.
    let (status, _) = call(
        &state,
        Method::GET,
        &path,
        &person_token(&key, &audience, stranger),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// A token holding `hub:publish`: a product's, or with `person` a person's through that product.
fn publishing_token(
    key: &ES256KeyPair,
    audience: &str,
    client: &str,
    person: Option<Uuid>,
) -> String {
    let mut custom = json!({ "client_id": client, "scope": hub::auth::PUBLISH });
    if person.is_some() {
        custom["auth_time"] = 1_700_000_000.into();
    }
    let subject = person.map_or_else(|| client.to_owned(), |p| p.to_string());
    let claims = Claims::with_custom_claims(custom, Duration::from_mins(5))
        .with_issuer(ISSUER)
        .with_audience(audience)
        .with_subject(subject);
    let options = HeaderOptions {
        signature_type: Some("at+jwt".to_owned()),
        ..Default::default()
    };
    key.sign_with_options(claims, &options).unwrap()
}

#[tokio::test]
async fn a_product_asks_who_is_online_and_invisible_is_not() {
    let Some(mut state) = common::online_state().await else {
        return;
    };
    let key = ES256KeyPair::generate().with_key_id("k1");
    let mut jwk: Value = serde_json::from_str(&key.public_key().to_jwk()).unwrap();
    jwk["kid"] = "k1".into();
    let audience = state.config.public_origin.clone();
    state.verifier = Some(Verifier::new(
        ISSUER,
        audience.clone(),
        KeySet::fixed(&json!({ "keys": [jwk] })),
    ));

    // Pat is connected, Sam connected and away, Ada connected and invisible, Lou not connected.
    let (pat, sam, ada, lou) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    for account in [pat, sam, ada] {
        presence::arrived(&state, account).await;
    }
    for (account, choice) in [(sam, "away"), (ada, "invisible")] {
        let (status, _) = call(
            &state,
            Method::PUT,
            "/api/v1/me/presence",
            &person_token(&key, &audience, account),
            Some(json!({ "choice": choice })),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    let path = "/api/v1/presence/online";
    let asked = json!({ "accounts": [lou, ada, sam, pat, sam] });
    let (status, answer) = call(
        &state,
        Method::POST,
        path,
        &publishing_token(&key, &audience, "chat", None),
        Some(asked.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(
        answer,
        json!({ "online": [sam, pat] }),
        "in the order asked, once each, away counted, invisible and absent not"
    );

    // A person's token never asks, even one carrying the scope.
    let (status, _) = call(
        &state,
        Method::POST,
        path,
        &publishing_token(&key, &audience, "chat", Some(pat)),
        Some(asked),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // More than 5,000 accounts: refused.
    let many: Vec<Uuid> = (0..=presence::MAX_ASKED).map(|_| Uuid::new_v4()).collect();
    let (status, problem) = call(
        &state,
        Method::POST,
        path,
        &publishing_token(&key, &audience, "chat", None),
        Some(json!({ "accounts": many })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(problem["reason"], "accounts", "{problem}");

    for account in [pat, sam, ada] {
        presence::departed(&state, account).await;
    }
}

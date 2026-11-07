//! Server events between services (design `docs/design/hub.md`, section 7; ADR 0011 and 0015 of
//! `Ruchoir/.github`).
//!
//! - **Keys.** Every service's Ed25519 event keys come from Propolis (`/.well-known/ruchoir-event-keys`),
//!   each naming the service that owns it; the set is cached and fetched again for an unknown key,
//!   at most once a minute.
//! - **Subscriptions.** Which service receives which kinds, at which endpoint, come from the file the
//!   installer writes (`HUB_SUBSCRIPTIONS`), applied whole at start. An endpoint must be on one of the
//!   instance's own hosts (`HUB_PAGE_ORIGINS`).
//! - **Intake** (`POST /api/v1/events`, a product with `hub:publish`): the signature is checked first,
//!   then the event's `source` against the key's owner and the caller, and its kind against the
//!   source's prefix. The exact bytes and signature are stored for every subscriber, and only then
//!   is the emitter answered `202`.
//! - **Delivery.** A worker posts the bytes unchanged with their `Ruchoir-Signature`, following no
//!   redirect, retrying with backoff for a day; the event's id makes a second delivery harmless.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::Router;
use base64::Engine as _;
use ruchoir_events::signature::{verify_and_parse, PublicKeys, SIGNATURE_HEADER};
use ruchoir_http::Problem;
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, DbErr, Statement, TransactionTrait};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::RwLock;
use tokio::time::Instant;
use uuid::Uuid;

use crate::auth::Publisher;
use crate::AppState;

/// The waits between attempts, then the last one again until the day is out.
const BACKOFF_SECS: [i64; 7] = [10, 30, 60, 300, 900, 1800, 3600];
/// The shortest wait between two fetches of the keys.
const KEYS_MIN_INTERVAL: Duration = Duration::from_secs(60);
/// The largest event the hub stores.
pub const MAX_EVENT_BYTES: usize = 256 * 1024;

fn internal(error: DbErr) -> Problem {
    tracing::error!(%error, "events store");
    Problem::internal()
}

/// Every service's event keys, from Propolis.
#[derive(Clone)]
pub struct EventKeys {
    uri: Option<String>,
    http: Option<reqwest::Client>,
    cache: Arc<RwLock<(PublicKeys, Option<Instant>)>>,
}

/// The keys of a published set: Ed25519 keys, each under its `kid`, owned by its `client_id`.
pub fn parse_keys(set: &Value) -> PublicKeys {
    let mut keys = PublicKeys::new();
    for jwk in set["keys"].as_array().into_iter().flatten() {
        let (Some(kid), Some(owner), Some(x)) = (
            jwk["kid"].as_str(),
            jwk["client_id"].as_str(),
            jwk["x"].as_str(),
        ) else {
            continue;
        };
        if jwk["kty"] != "OKP" || jwk["crv"] != "Ed25519" {
            continue;
        }
        let Ok(public) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(x) else {
            continue;
        };
        let _ = keys.insert(kid, owner, &public);
    }
    keys
}

impl EventKeys {
    /// The keys Propolis at `issuer` publishes.
    pub fn of_issuer(issuer: &str, http: reqwest::Client) -> Self {
        Self {
            uri: Some(format!(
                "{}/.well-known/ruchoir-event-keys",
                issuer.trim_end_matches('/')
            )),
            http: Some(http),
            cache: Arc::default(),
        }
    }

    /// A set that never fetches: for tests, and without an issuer (nothing verifies).
    pub fn fixed(keys: PublicKeys) -> Self {
        Self {
            uri: None,
            http: None,
            cache: Arc::new(RwLock::new((keys, Some(Instant::now())))),
        }
    }

    /// The keys, fetched again when `kid` is unknown and the last fetch is a minute old.
    pub async fn for_key(&self, kid: &str) -> PublicKeys {
        {
            let cache = self.cache.read().await;
            let recent = cache.1.is_some_and(|at| at.elapsed() < KEYS_MIN_INTERVAL);
            if cache.0.contains(kid) || recent || self.http.is_none() {
                return cache.0.clone();
            }
        }
        let mut cache = self.cache.write().await;
        if cache.1.is_none_or(|at| at.elapsed() >= KEYS_MIN_INTERVAL) {
            if let (Some(http), Some(uri)) = (&self.http, &self.uri) {
                let fetched = async {
                    http.get(uri)
                        .send()
                        .await?
                        .error_for_status()?
                        .json::<Value>()
                        .await
                };
                // On failure, keep what was known: Propolis being down must not refuse every event.
                if let Ok(set) = fetched.await {
                    cache.0 = parse_keys(&set);
                }
            }
            cache.1 = Some(Instant::now());
        }
        cache.0.clone()
    }
}

/// The subscriptions file the installer writes.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Subscriptions {
    #[serde(default, rename = "subscription")]
    pub subscriptions: Vec<Subscription>,
}

/// What one service receives, and where.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Subscription {
    /// The service, as its events name it (`calendar`).
    pub service: String,
    /// Its delivery endpoint, on its own host (`https://calendar.example.org/api/v1/events`).
    pub endpoint: String,
    /// Exact kinds (`drive.file.deleted`), or every kind of a namespace (`propolis.*`).
    pub kinds: Vec<String>,
}

impl Subscriptions {
    /// Read and check the file against the instance's own origins.
    pub fn parse(text: &str, origins: &[String]) -> Result<Self, String> {
        let file: Subscriptions =
            toml::from_str(text).map_err(|e| format!("subscriptions: {e}"))?;
        for sub in &file.subscriptions {
            let on_instance = origins
                .iter()
                .any(|origin| sub.endpoint.starts_with(&format!("{origin}/")));
            if !secure(&sub.endpoint) || !on_instance {
                return Err(format!(
                    "subscriptions: {}: {} is not on one of the instance's hosts",
                    sub.service, sub.endpoint
                ));
            }
            for kind in &sub.kinds {
                let pattern = kind.strip_suffix(".*").unwrap_or(kind);
                if ruchoir_events::Kind::new(format!("{pattern}.x")).is_err()
                    && ruchoir_events::Kind::new(kind.clone()).is_err()
                {
                    return Err(format!(
                        "subscriptions: {}: {kind:?} is not a kind",
                        sub.service
                    ));
                }
            }
        }
        Ok(file)
    }

    /// Replace the stored subscriptions with these, in one transaction.
    pub async fn apply(&self, db: &DatabaseConnection) -> Result<(), DbErr> {
        let txn = db.begin().await?;
        txn.execute_unprepared("DELETE FROM event_subscriptions")
            .await?;
        for sub in &self.subscriptions {
            let kinds: BTreeSet<&String> = sub.kinds.iter().collect();
            for kind in kinds {
                txn.execute_raw(Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    "INSERT INTO event_subscriptions (subscriber, kind, endpoint) VALUES ($1, $2, $3)",
                    [sub.service.clone().into(), kind.clone().into(), sub.endpoint.clone().into()],
                ))
                .await?;
            }
        }
        txn.commit().await
    }
}

/// An endpoint the hub may post to: `https`, or plain `http` to the machine itself (a development
/// instance, the tests).
fn secure(endpoint: &str) -> bool {
    endpoint.starts_with("https://")
        || endpoint.starts_with("http://127.0.0.1:")
        || endpoint.starts_with("http://localhost:")
}

/// Whether a subscribed `pattern` covers `kind`.
pub fn matches(pattern: &str, kind: &str) -> bool {
    match pattern.strip_suffix(".*") {
        Some(namespace) => kind.starts_with(namespace) && kind[namespace.len()..].starts_with('.'),
        None => pattern == kind,
    }
}

/// Take an event from its emitter.
#[utoipa::path(
    post,
    path = "/api/v1/events",
    tag = "events",
    request_body(content = Object, description = "A ruchoir-events Event, signed in the Ruchoir-Signature header"),
    responses(
        (status = 202, description = "Stored for every subscriber"),
        (status = 400, description = "Refused (`reason`: `signature`, `source`, `kind_not_yours`, `too_large`)"),
        (status = 401, description = "No valid token"),
        (status = 403, description = "Not a product with `hub:publish`")
    )
)]
pub async fn intake(
    State(state): State<AppState>,
    publisher: Publisher,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, Problem> {
    if body.len() > MAX_EVENT_BYTES {
        return Err(Problem::bad_request().with_reason("too_large"));
    }
    let signature = headers
        .get(SIGNATURE_HEADER)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| Problem::bad_request().with_reason("signature"))?;
    let kid = ruchoir_events::signature::SignatureHeader::parse(signature)
        .map_err(|_| Problem::bad_request().with_reason("signature"))?
        .key_id()
        .to_owned();
    let keys = state.event_keys.for_key(&kid).await;
    // The signature first, the body only then; the hub never signs or changes an event.
    let event = verify_and_parse::<Value>(&body, signature, &keys).map_err(|why| {
        tracing::info!(%why, "an event refused");
        Problem::bad_request().with_reason("signature")
    })?;
    // The token, the key and the event all name the same service, and the kind is its own.
    if event.source != publisher.client_id {
        return Err(Problem::bad_request().with_reason("source"));
    }
    if !event
        .kind
        .as_str()
        .starts_with(&format!("{}.", event.source))
    {
        return Err(Problem::bad_request().with_reason("kind_not_yours"));
    }
    let rows = state
        .db
        .query_all_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT subscriber, kind, endpoint FROM event_subscriptions",
        ))
        .await
        .map_err(internal)?;
    let kind = event.kind.as_str();
    let mut targets: Vec<(String, String)> = Vec::new();
    for row in rows {
        let pattern: String = row.try_get("", "kind").map_err(internal)?;
        let subscriber: String = row.try_get("", "subscriber").map_err(internal)?;
        let endpoint: String = row.try_get("", "endpoint").map_err(internal)?;
        // An emitter is never sent its own events back.
        if matches(&pattern, kind)
            && subscriber != event.source
            && !targets.iter().any(|(s, _)| *s == subscriber)
        {
            targets.push((subscriber, endpoint));
        }
    }
    for (subscriber, endpoint) in targets {
        state
            .db
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "INSERT INTO event_deliveries (id, event_id, kind, subscriber, endpoint, body, signature) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT (event_id, subscriber) DO NOTHING",
                [
                    Uuid::new_v4().into(),
                    event.id.into(),
                    kind.into(),
                    subscriber.into(),
                    endpoint.into(),
                    body.to_vec().into(),
                    signature.into(),
                ],
            ))
            .await
            .map_err(internal)?;
    }
    if event.source == "propolis" {
        // Something changed in the organisations: read the feed now rather than in a minute.
        if let Some(replica) = &state.replica {
            replica.wake.notify_one();
        }
        // Signing out everywhere ends every socket of the person, on every process.
        if kind == "propolis.account.signed_out_everywhere" {
            if let Some(account) = event.payload["account"]
                .as_str()
                .and_then(|a| a.parse().ok())
            {
                state.fanout.close_account(account).await;
            }
        }
    }
    Ok(StatusCode::ACCEPTED)
}

/// What one round of deliveries did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Round {
    pub delivered: usize,
    pub failed: usize,
}

/// Deliver the events that are due.
pub async fn deliver_due(db: &DatabaseConnection, http: &reqwest::Client) -> Result<Round, DbErr> {
    // Leased for two minutes, so two processes never send the same one at once.
    let due = db
        .query_all_raw(Statement::from_string(
            DbBackend::Postgres,
            "UPDATE event_deliveries SET next_attempt_at = now() + interval '2 minutes' \
             WHERE id IN (SELECT id FROM event_deliveries WHERE delivered_at IS NULL \
                 AND next_attempt_at <= now() AND expires_at > now() \
                 ORDER BY next_attempt_at LIMIT 100 FOR UPDATE SKIP LOCKED) \
             RETURNING id, subscriber, endpoint, body, signature, attempts",
        ))
        .await?;
    let mut round = Round::default();
    for row in due {
        let id: Uuid = row.try_get("", "id")?;
        let subscriber: String = row.try_get("", "subscriber")?;
        let endpoint: String = row.try_get("", "endpoint")?;
        let body: Vec<u8> = row.try_get("", "body")?;
        let signature: String = row.try_get("", "signature")?;
        let attempts: i32 = row.try_get("", "attempts")?;
        let outcome = http
            .post(&endpoint)
            .header("content-type", "application/json")
            .header(SIGNATURE_HEADER, signature)
            .body(body)
            .send()
            .await;
        let failure = match outcome {
            Ok(response) if response.status().is_success() => None,
            Ok(response) => Some(format!("answered {}", response.status().as_u16())),
            Err(error) => Some(error.without_url().to_string()),
        };
        match failure {
            None => {
                db.execute_raw(Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    "UPDATE event_deliveries SET delivered_at = now(), attempts = attempts + 1, last_error = NULL WHERE id = $1",
                    [id.into()],
                ))
                .await?;
                round.delivered += 1;
            }
            Some(error) => {
                let wait = BACKOFF_SECS[usize::try_from(attempts)
                    .unwrap_or(0)
                    .min(BACKOFF_SECS.len() - 1)];
                tracing::warn!(%subscriber, %error, "an event not delivered");
                db.execute_raw(Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    "UPDATE event_deliveries SET attempts = attempts + 1, last_error = $2, \
                     next_attempt_at = now() + make_interval(secs => $3) WHERE id = $1",
                    [id.into(), error.into(), (wait as f64).into()],
                ))
                .await?;
                round.failed += 1;
            }
        }
    }
    Ok(round)
}

/// Deliver every few seconds for as long as the hub runs; log what a day could not deliver.
pub fn spawn(db: DatabaseConnection, http: reqwest::Client) {
    tokio::spawn(async move {
        let mut every = tokio::time::interval(Duration::from_secs(5));
        loop {
            every.tick().await;
            if let Err(error) = deliver_due(&db, &http).await {
                tracing::error!(%error, "event deliveries");
            }
            let given_up = db
                .execute_raw(Statement::from_string(
                    DbBackend::Postgres,
                    "DELETE FROM event_deliveries WHERE delivered_at IS NULL AND expires_at <= now()",
                ))
                .await;
            if let Ok(result) = given_up {
                if result.rows_affected() > 0 {
                    tracing::error!(
                        count = result.rows_affected(),
                        "events given up after a day"
                    );
                }
            }
        }
    });
}

/// The events route.
pub fn router() -> Router<AppState> {
    Router::new().route("/api/v1/events", post(intake))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pattern_covers_its_namespace_or_its_kind() {
        assert!(matches("drive.file.deleted", "drive.file.deleted"));
        assert!(!matches("drive.file.deleted", "drive.file.created"));
        assert!(matches("propolis.*", "propolis.member.added"));
        assert!(!matches("propolis.*", "propolisx.member.added"));
        assert!(!matches("drive.*", "chat.message.created"));
    }

    #[test]
    fn a_subscription_stays_on_the_instance() {
        let origins = vec!["https://calendar.example.org".to_owned()];
        let good = r#"
[[subscription]]
service = "calendar"
endpoint = "https://calendar.example.org/api/v1/events"
kinds = ["propolis.*", "drive.file.deleted"]
"#;
        assert_eq!(
            Subscriptions::parse(good, &origins)
                .unwrap()
                .subscriptions
                .len(),
            1
        );
        for bad in [
            good.replace(
                "https://calendar.example.org/api",
                "https://evil.example.org/api",
            ),
            good.replace(
                "https://calendar.example.org/api",
                "http://calendar.example.org/api",
            ),
            good.replace("\"drive.file.deleted\"", "\"Drive\""),
            format!("{good}\nsurprise = true\n"),
        ] {
            assert!(Subscriptions::parse(&bad, &origins).is_err(), "{bad}");
        }
    }
}

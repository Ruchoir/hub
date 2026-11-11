//! Web Push: telling a browser with no Ruchoir page open that something is waiting.
//!
//! **Nothing about the entry leaves the instance.** A push carries only a constant marker, encrypted
//! for the subscription ([`super::ece`]), and serves to wake the page's service worker. The worker
//! then asks its product's backend, on the product's own host and with the product's cookie, which
//! asks the hub ([`pending`]) with a token exchanged for the person (decision of 2026-11-08): no
//! token in the page. The push service of the browser's vendor, which the instance does not choose
//! (ADR 0001), learns that one of its subscribers received something at a given time, and nothing
//! else.
//!
//! **Only known push services are ever called.** A subscription's endpoint is a URL the browser hands
//! over, which means a URL the client chose; posting to whatever it names would let anyone with an
//! account make the hub send requests into its own network. Endpoints are therefore checked against
//! `HUB_PUSH_HOSTS` when they are stored, and again when they are used.

use std::collections::HashMap;

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::Json;
use fred::interfaces::KeysInterface;
use fred::types::{Expiration, SetOptions};
use ruchoir_http::Problem;
use sea_orm::{ConnectionTrait, DbBackend, DbErr, Statement};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use time::OffsetDateTime;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::vapid::VapidKeys;
use super::words::{self, Locale};
use crate::auth::Person;
use crate::AppState;

/// How long a push service keeps a push for a device that is offline. A day: past that, what it
/// would announce is in the inbox and, most likely, already in a digest.
const PUSH_TTL_SECS: u32 = 24 * 60 * 60;

/// Consecutive failures after which a subscription is dropped. A browser that was uninstalled
/// usually answers `410` at once; this catches the ones whose service only ever errors.
const MAX_FAILURES: i32 = 10;

/// How far back a woken worker is shown entries from.
const PENDING_WINDOW_SECS: i64 = 24 * 60 * 60;

/// How many entries a single wake-up draws. More than that is a backlog, and the inbox is the place
/// for a backlog.
const PENDING_LIMIT: usize = 5;

/// How long a requested test waits for the worker to collect it.
pub(super) const TEST_TTL_SECS: i64 = 5 * 60;

/// The Valkey key marking that the next wake-up of `account`'s devices should draw the test.
pub(super) fn test_key(account: Uuid) -> String {
    format!("push:test:{account}")
}

/// The Valkey key holding back a second test for a few seconds.
pub(super) fn throttle_key(account: Uuid) -> String {
    format!("push:test-throttle:{account}")
}

pub(super) fn internal(error: DbErr) -> Problem {
    tracing::error!(%error, "push store");
    Problem::internal()
}

/// `409`, with why the hub cannot do it.
pub(super) fn conflict(reason: &'static str) -> Problem {
    Problem::conflict().with_reason(reason)
}

/// Whether a subscription endpoint may be called, and the origin to sign for when it may.
///
/// The endpoint must be `https`, carry no credentials, and name one of `allowed` or a subdomain of
/// it (`web.push.apple.com` for `push.apple.com`).
pub fn endpoint_audience(endpoint: &str, allowed: &[String]) -> Option<String> {
    let uri: axum::http::Uri = endpoint.parse().ok()?;
    if uri.scheme_str() != Some("https") {
        return None;
    }
    let authority = uri.authority()?;
    if authority.as_str().contains('@') {
        return None;
    }
    let host = authority.host().to_ascii_lowercase();
    let known = allowed.iter().any(|suffix| {
        let suffix = suffix.trim().trim_start_matches('.').to_ascii_lowercase();
        !suffix.is_empty() && (host == suffix || host.ends_with(&format!(".{suffix}")))
    });
    known.then(|| match authority.port_u16() {
        Some(port) if port != 443 => format!("https://{host}:{port}"),
        _ => format!("https://{host}"),
    })
}

/// How the push service can reach whoever runs this instance, as RFC 8292 asks: the hub's public
/// address when it is `https`.
fn subject(state: &AppState) -> String {
    let origin = &state.config.public_origin;
    if origin.starts_with("https://") {
        origin.clone()
    } else {
        "mailto:no-reply@localhost".to_owned()
    }
}

/// Whether the instance offers Web Push, and the key a browser subscribes against.
#[derive(Debug, Serialize, ToSchema)]
pub struct PushConfig {
    /// False when the instance has no VAPID key.
    pub available: bool,
    /// The VAPID public key (`applicationServerKey`), base64url. Absent when not available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_key: Option<String>,
}

/// Whether this instance sends Web Push, and with which key.
#[utoipa::path(
    get,
    path = "/api/v1/push/config",
    tag = "push",
    responses((status = 200, description = "Web Push availability", body = PushConfig))
)]
pub async fn config(State(state): State<AppState>, _person: Person) -> Json<PushConfig> {
    let public_key = state.push.vapid.as_ref().map(|k| k.public_key.clone());
    Json(PushConfig {
        available: public_key.is_some(),
        public_key,
    })
}

/// A browser's subscription keys, as `PushSubscription.toJSON()` lays them out.
#[derive(Debug, Deserialize, ToSchema)]
pub struct SubscriptionKeys {
    pub p256dh: String,
    pub auth: String,
}

/// A browser's subscription, as `PushSubscription.toJSON()` lays it out.
#[derive(Debug, Deserialize, ToSchema)]
pub struct Subscription {
    pub endpoint: String,
    pub keys: SubscriptionKeys,
    /// The browser, as its product read its `User-Agent`, shown in the person's device list.
    #[serde(default)]
    pub user_agent: Option<String>,
}

/// A subscription to forget, named by its endpoint.
#[derive(Debug, Deserialize, ToSchema)]
pub struct Unsubscription {
    pub endpoint: String,
}

/// A base64url value of plausible length, which is all a key can be checked for before it is used.
fn plausible_key(value: &str) -> bool {
    (16..=256).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'=')
}

/// Remember a browser's subscription for the person.
///
/// Keyed by endpoint: the same browser subscribing again refreshes its row, and a browser someone
/// else signed in on is handed to the caller rather than notifying two accounts.
#[utoipa::path(
    put,
    path = "/api/v1/me/push/subscription",
    tag = "push",
    request_body = Subscription,
    responses(
        (status = 204, description = "Subscription stored"),
        (status = 400, description = "Not a push service this instance calls (`reason`: `endpoint`, `keys`)"),
        (status = 409, description = "The instance sends no Web Push (`reason`: `push_off`)")
    )
)]
pub async fn subscribe(
    State(state): State<AppState>,
    person: Person,
    Json(body): Json<Subscription>,
) -> Result<StatusCode, Problem> {
    if state.push.vapid.is_none() {
        return Err(conflict("push_off"));
    }
    if body.endpoint.len() > 2048
        || endpoint_audience(&body.endpoint, &state.config.push_hosts).is_none()
    {
        return Err(Problem::bad_request().with_reason("endpoint"));
    }
    if !plausible_key(&body.keys.p256dh) || !plausible_key(&body.keys.auth) {
        return Err(Problem::bad_request().with_reason("keys"));
    }
    let user_agent = body
        .user_agent
        .map(|value| value.chars().take(256).collect::<String>());
    state
        .db
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "INSERT INTO push_subscriptions (id, account_id, endpoint, p256dh, auth, user_agent) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (endpoint) DO UPDATE SET account_id = EXCLUDED.account_id, \
             p256dh = EXCLUDED.p256dh, auth = EXCLUDED.auth, user_agent = EXCLUDED.user_agent, failures = 0",
            [
                Uuid::new_v4().into(),
                person.account.into(),
                body.endpoint.into(),
                body.keys.p256dh.into(),
                body.keys.auth.into(),
                user_agent.into(),
            ],
        ))
        .await
        .map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Forget a browser's subscription. Idempotent, and only ever the person's own.
#[utoipa::path(
    delete,
    path = "/api/v1/me/push/subscription",
    tag = "push",
    request_body = Unsubscription,
    responses((status = 204, description = "Forgotten (or was not known)"))
)]
pub async fn unsubscribe(
    State(state): State<AppState>,
    person: Person,
    Json(body): Json<Unsubscription>,
) -> Result<StatusCode, Problem> {
    state
        .db
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "DELETE FROM push_subscriptions WHERE endpoint = $1 AND account_id = $2",
            [body.endpoint.into(), person.account.into()],
        ))
        .await
        .map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

/// What a woken device should draw, since when, in which language.
#[derive(Debug, Deserialize, IntoParams)]
pub struct PendingQuery {
    /// Only entries created after this instant (RFC 3339): the newest one this device has already
    /// shown. Absent, the last ten minutes.
    pub since: Option<String>,
    /// The person's language, as their product knows it (`en`). Absent, the request's
    /// `Accept-Language`, then French.
    pub locale: Option<String>,
}

/// One entry, worded for a tray or a lock screen.
#[derive(Debug, Serialize, ToSchema)]
pub struct PushItem {
    pub id: Uuid,
    /// The product that published it (`chat`); absent for the test.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub product: Option<String>,
    pub kind: String,
    pub category: String,
    /// Where a tap leads; absent for the test.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    pub title: String,
    pub body: String,
    /// RFC 3339.
    pub created_at: String,
}

/// What a wake-up draws.
#[derive(Debug, Serialize, ToSchema)]
pub struct Pending {
    /// Whether to draw them without a sound (the person's own setting).
    pub silent: bool,
    /// Newest first, at most a handful.
    pub items: Vec<PushItem>,
}

fn rfc3339(at: OffsetDateTime) -> String {
    at.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// What a woken service worker or phone should draw.
///
/// The same rule as the inbox ([`crate::prefs::allows`]), so what a setting silenced is not drawn
/// even when something else woke the device, and only what is still unread: an entry opened on
/// another device in the meantime has nothing left to say.
#[utoipa::path(
    get,
    path = "/api/v1/me/push/pending",
    tag = "push",
    params(PendingQuery),
    responses((status = 200, description = "What to draw", body = Pending))
)]
pub async fn pending(
    State(state): State<AppState>,
    person: Person,
    headers: HeaderMap,
    Query(query): Query<PendingQuery>,
) -> Result<Json<Pending>, Problem> {
    let now = OffsetDateTime::now_utc();
    let floor = now - time::Duration::seconds(PENDING_WINDOW_SECS);
    let since = query
        .since
        .as_deref()
        .and_then(|value| {
            OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339).ok()
        })
        .unwrap_or(now - time::Duration::minutes(10))
        .max(floor);
    let locale = Locale::parse(query.locale.as_deref().or_else(|| {
        headers
            .get(header::ACCEPT_LANGUAGE)
            .and_then(|v| v.to_str().ok())
    }));

    let rows = state
        .db
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT id, product, kind, category, organisation_id, link, variables, created_at \
             FROM inbox_entries WHERE account_id = $1 AND read_at IS NULL AND created_at > $2 \
             ORDER BY created_at DESC LIMIT 50",
            [person.account.into(), since.into()],
        ))
        .await
        .map_err(internal)?;

    // A test asked for from the preferences, collected once by whichever device wakes first.
    let test: Option<String> = state
        .valkey
        .getdel(test_key(person.account))
        .await
        .unwrap_or(None);

    let prefs = crate::prefs::load(&state.db, person.account)
        .await
        .map_err(internal)?;
    let mut levels: HashMap<Uuid, Option<String>> = HashMap::new();
    let mut items = Vec::new();
    if test.is_some() {
        items.push(PushItem {
            id: Uuid::nil(),
            product: None,
            kind: "hub.test".to_owned(),
            category: "notice".to_owned(),
            link: None,
            title: "Ruchoir".to_owned(),
            body: words::test_body(locale).to_owned(),
            created_at: rfc3339(now),
        });
    }
    for row in rows {
        if items.len() >= PENDING_LIMIT {
            break;
        }
        let read = |name: &str| row.try_get::<String>("", name);
        let category = read("category").map_err(internal)?;
        let organisation: Option<Uuid> = row.try_get("", "organisation_id").map_err(internal)?;
        let level = match organisation {
            Some(organisation) => match levels.get(&organisation) {
                Some(level) => level.clone(),
                None => {
                    let level = crate::prefs::level(&state.db, person.account, organisation)
                        .await
                        .map_err(internal)?;
                    levels.insert(organisation, level.clone());
                    level
                }
            },
            None => None,
        };
        if !crate::prefs::allows(
            &category,
            &prefs,
            level.as_deref(),
            crate::prefs::Delivery::App,
        ) {
            continue;
        }
        let variables: Value = row.try_get("", "variables").map_err(internal)?;
        let variables = variables.as_object().cloned().unwrap_or_default();
        let (title, body) = words::tray(locale, &category, &variables);
        let created_at: OffsetDateTime = row.try_get("", "created_at").map_err(internal)?;
        items.push(PushItem {
            id: row.try_get("", "id").map_err(internal)?,
            product: Some(read("product").map_err(internal)?),
            kind: read("kind").map_err(internal)?,
            category,
            link: Some(read("link").map_err(internal)?),
            title,
            body,
            created_at: rfc3339(created_at),
        });
    }
    Ok(Json(Pending {
        silent: !prefs.sound,
        items,
    }))
}

/// One browser's subscription, as stored.
pub(super) struct Stored {
    id: Uuid,
    endpoint: String,
    p256dh: String,
    auth: String,
    failures: i32,
}

/// The browsers an account subscribed.
pub(super) async fn subscriptions(
    db: &impl ConnectionTrait,
    account: Uuid,
) -> Result<Vec<Stored>, DbErr> {
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT id, endpoint, p256dh, auth, failures FROM push_subscriptions WHERE account_id = $1",
            [account.into()],
        ))
        .await?;
    rows.iter()
        .map(|row| {
            Ok(Stored {
                id: row.try_get("", "id")?,
                endpoint: row.try_get("", "endpoint")?,
                p256dh: row.try_get("", "p256dh")?,
                auth: row.try_get("", "auth")?,
                failures: row.try_get("", "failures")?,
            })
        })
        .collect()
}

/// Announce fresh entries to their recipients' browsers and phones, in the background.
///
/// Called once the entries are stored, for the recipients who want them in the app (the inbox
/// applied their settings). Never fails the publication: a push that cannot be sent is logged, and
/// the inbox has the entry anyway.
pub fn dispatch(state: &AppState, recipients: Vec<Uuid>, category: String) {
    if recipients.is_empty() {
        return;
    }
    let state = state.clone();
    tokio::spawn(async move {
        if let Err(error) = deliver(&state, recipients, &category).await {
            tracing::warn!(%error, "could not send pushes");
        }
    });
}

/// Send one push per subscription and wake each phone of every recipient who may be interrupted.
async fn deliver(state: &AppState, recipients: Vec<Uuid>, category: &str) -> Result<(), DbErr> {
    let now = OffsetDateTime::now_utc();
    // Being named, written to directly or reminded is worth waking a phone for; the rest can wait for
    // the device's next scheduled check.
    let urgency = if matches!(category, "mention" | "direct" | "reminder") {
        "high"
    } else {
        "normal"
    };
    for account in recipients {
        let browsers = match &state.push.vapid {
            Some(_) => subscriptions(&state.db, account).await?,
            None => Vec::new(),
        };
        let phones = if state.config.push_relay_url.is_some() {
            super::phone::devices(&state.db, account, now).await?
        } else {
            Vec::new()
        };
        if browsers.is_empty() && phones.is_empty() {
            continue;
        }
        let prefs = crate::prefs::load(&state.db, account).await?;
        let choice = crate::live::presence::choice_of(&state.db, account).await?;
        if !crate::prefs::may_interrupt(&prefs, choice.as_deref(), now) {
            continue;
        }
        for phone in phones {
            super::phone::wake(state, phone).await;
        }
        if let Some(keys) = &state.push.vapid {
            for browser in browsers {
                send_one(state, keys, browser, urgency, now).await;
            }
        }
    }
    Ok(())
}

/// What a push service answered.
enum Outcome {
    Delivered,
    /// The subscription is gone for good (unsubscribed, expired, or made against another key).
    Gone,
    Failed,
}

/// Send one push; `true` when the push service accepted it.
pub(super) async fn send_one(
    state: &AppState,
    keys: &VapidKeys,
    subscription: Stored,
    urgency: &'static str,
    now: OffsetDateTime,
) -> bool {
    // Checked again at send time: the allowlist may have been narrowed since this was stored.
    let Some(audience) = endpoint_audience(&subscription.endpoint, &state.config.push_hosts) else {
        forget(state, subscription.id).await;
        return false;
    };
    // Keys that cannot be encrypted for were never a working subscription.
    let Some(body) =
        super::ece::encrypt(super::ece::WAKE, &subscription.p256dh, &subscription.auth)
    else {
        tracing::warn!(subscription = %subscription.id, "unusable push subscription keys; dropped");
        forget(state, subscription.id).await;
        return false;
    };
    let authorization = keys.authorization(&audience, &subject(state), now);
    let sent = state
        .push
        .http
        .post(&subscription.endpoint)
        .header("Authorization", authorization)
        .header("TTL", PUSH_TTL_SECS.to_string())
        .header("Urgency", urgency)
        // No `Topic` header. It would collapse pushes queued for an offline device into one, but
        // Apple refuses the push outright with a topic (`400 BadWebPushTopic`), and nothing is lost
        // without it: the worker draws everything unread whenever it wakes.
        .header("Content-Encoding", "aes128gcm")
        .header("Content-Type", "application/octet-stream")
        .body(body)
        .send()
        .await;
    let outcome = match sent {
        Ok(response) => {
            let status = response.status().as_u16();
            // The push services say why they refuse in the body (`{"reason": ...}` at Apple, a
            // sentence elsewhere): kept, short, for the log.
            let reason = |response: reqwest::Response| async move {
                response
                    .text()
                    .await
                    .unwrap_or_default()
                    .chars()
                    .take(300)
                    .collect::<String>()
            };
            match status {
                200..=299 => Outcome::Delivered,
                // 404/410: expired or unsubscribed. 401/403: the push service no longer accepts the
                // key for it. Either way it will never work again.
                401 | 403 | 404 | 410 => {
                    let reason = reason(response).await;
                    tracing::info!(status, %reason, subscription = %subscription.id, "push subscription gone; dropped");
                    Outcome::Gone
                }
                _ => {
                    let reason = reason(response).await;
                    tracing::warn!(status, %reason, subscription = %subscription.id, "push service refused a push");
                    Outcome::Failed
                }
            }
        }
        Err(error) => {
            tracing::warn!(error = %error.without_url(), subscription = %subscription.id, "could not reach a push service");
            Outcome::Failed
        }
    };

    let (sql, delivered) = match outcome {
        Outcome::Delivered => (
            "UPDATE push_subscriptions SET last_success_at = now(), failures = 0 WHERE id = $1",
            true,
        ),
        Outcome::Failed if subscription.failures + 1 < MAX_FAILURES => (
            "UPDATE push_subscriptions SET failures = failures + 1 WHERE id = $1",
            false,
        ),
        Outcome::Gone | Outcome::Failed => {
            forget(state, subscription.id).await;
            return false;
        }
    };
    if let Err(error) = state
        .db
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            sql,
            [subscription.id.into()],
        ))
        .await
    {
        tracing::warn!(%error, "could not record a push outcome");
    }
    delivered
}

/// What a test push did.
#[derive(Debug, Serialize, ToSchema)]
pub struct PushTest {
    /// How many of the person's browsers are subscribed.
    pub subscriptions: usize,
    /// How many of those the push services accepted the test for.
    pub delivered: usize,
}

/// Hold back a second test for a few seconds, whichever way it was asked, and leave the test for
/// the first device that wakes.
pub(super) async fn arm_test(state: &AppState, account: Uuid) -> Result<(), Problem> {
    // One test every few seconds is plenty, and keeps the button from being a way to hammer a push
    // service or the relay from this hub.
    let throttle: Option<String> = state
        .valkey
        .set(
            throttle_key(account),
            "1",
            Some(Expiration::EX(5)),
            Some(SetOptions::NX),
            false,
        )
        .await
        .map_err(|_| Problem::internal())?;
    if throttle.is_none() {
        return Err(conflict("test_just_sent"));
    }
    let _: () = state
        .valkey
        .set(
            test_key(account),
            "1",
            Some(Expiration::EX(TEST_TTL_SECS)),
            None,
            false,
        )
        .await
        .map_err(|_| Problem::internal())?;
    Ok(())
}

/// Send a real push to each of the person's subscribed browsers.
///
/// The whole chain the entries take, on purpose: the hub, the vendor's push service, the service
/// worker waking up and asking for what to draw. Quiet hours and settings do not apply: it was asked
/// for.
#[utoipa::path(
    post,
    path = "/api/v1/me/push/test",
    operation_id = "test_web_push",
    tag = "push",
    responses(
        (status = 200, description = "Sent", body = PushTest),
        (status = 409, description = "Refused (`reason`: `push_off`, `no_browser`, `test_just_sent`)")
    )
)]
pub async fn test(
    State(state): State<AppState>,
    person: Person,
) -> Result<Json<PushTest>, Problem> {
    let Some(keys) = &state.push.vapid else {
        return Err(conflict("push_off"));
    };
    let browsers = subscriptions(&state.db, person.account)
        .await
        .map_err(internal)?;
    if browsers.is_empty() {
        return Err(conflict("no_browser"));
    }
    arm_test(&state, person.account).await?;
    let now = OffsetDateTime::now_utc();
    let total = browsers.len();
    let mut delivered = 0;
    for browser in browsers {
        if send_one(&state, keys, browser, "high", now).await {
            delivered += 1;
        }
    }
    Ok(Json(PushTest {
        subscriptions: total,
        delivered,
    }))
}

async fn forget(state: &AppState, id: Uuid) {
    if let Err(error) = state
        .db
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "DELETE FROM push_subscriptions WHERE id = $1",
            [id.into()],
        ))
        .await
    {
        tracing::warn!(%error, "could not drop a push subscription");
    }
}

/// The variables of an entry, for the words.
pub(crate) fn variables_of(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::endpoint_audience;

    fn hosts() -> Vec<String> {
        crate::config::DEFAULT_PUSH_HOSTS
            .split(',')
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn the_browsers_own_push_services_are_accepted() {
        let allowed = hosts();
        assert_eq!(
            endpoint_audience("https://fcm.googleapis.com/fcm/send/abc:def", &allowed).as_deref(),
            Some("https://fcm.googleapis.com")
        );
        assert_eq!(
            endpoint_audience("https://web.push.apple.com/QGx", &allowed).as_deref(),
            Some("https://web.push.apple.com")
        );
        assert_eq!(
            endpoint_audience(
                "https://updates.push.services.mozilla.com/wpush/v2/gAAA",
                &allowed
            )
            .as_deref(),
            Some("https://updates.push.services.mozilla.com")
        );
        assert!(endpoint_audience(
            "https://wns2-par02p.notify.windows.com/w/?token=x",
            &allowed
        )
        .is_some());
    }

    #[test]
    fn anything_else_is_refused() {
        let allowed = hosts();
        for endpoint in [
            // The hub's own network.
            "https://127.0.0.1/hook",
            "https://localhost/hook",
            "https://169.254.169.254/latest/meta-data",
            // A look-alike that only ends with an allowed name.
            "https://evilfcm.googleapis.com.attacker.test/x",
            "https://notfcm.googleapis.com/x",
            // Plain HTTP, credentials, garbage.
            "http://fcm.googleapis.com/fcm/send/abc",
            "https://user:pass@fcm.googleapis.com/fcm/send/abc",
            "not a url",
        ] {
            assert!(
                endpoint_audience(endpoint, &allowed).is_none(),
                "{endpoint} must be refused"
            );
        }
    }

    #[test]
    fn a_non_default_port_is_part_of_the_audience() {
        let allowed = vec!["push.example".to_owned()];
        assert_eq!(
            endpoint_audience("https://push.example:8443/x", &allowed).as_deref(),
            Some("https://push.example:8443")
        );
    }
}

//! Phone notifications (ADR 0002): the phones that allowed them, and waking them through the
//! publisher's relay.
//!
//! The instance never holds Apple's or Google's keys. It posts a device's token to the relay
//! (`HUB_PUSH_RELAY_URL`), which sends a constant push; the phone then asks the hub what to show
//! (`GET /api/v1/me/push/pending`, with its own token for the hub), as a browser's worker does
//! through its product. Which entries wake a phone is decided where the browsers' are
//! ([`super::web`]): same settings, same quiet hours.

use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use fred::interfaces::KeysInterface;
use fred::types::{Expiration, SetOptions};
use ruchoir_http::Problem;
use sea_orm::{ConnectionTrait, DbBackend, DbErr, Statement};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;

use super::web::{conflict, internal};
use crate::auth::Person;
use crate::AppState;

/// A phone unseen this long (its app not opened) is forgotten.
const STALE_DAYS: i64 = 60;

/// A phone, as its app registers it.
#[derive(Debug, Deserialize, ToSchema)]
pub struct DeviceRequest {
    /// `ios` or `android`.
    pub platform: String,
    /// The device token Apple or Google handed the app.
    pub token: String,
}

/// A phone to forget.
#[derive(Debug, Deserialize, ToSchema)]
pub struct DeviceToken {
    pub token: String,
}

/// A phone, as stored.
#[derive(Debug, Clone)]
pub struct Device {
    pub id: Uuid,
    pub platform: String,
    pub token: String,
}

/// Whether `token` looks like what `platform`'s vendor hands out: hex from Apple, a short
/// alphabet from Google. Checked here so the relay is never asked about anything else.
pub fn plausible(platform: &str, token: &str) -> bool {
    match platform {
        "ios" => (32..=400).contains(&token.len()) && token.bytes().all(|b| b.is_ascii_hexdigit()),
        "android" => {
            (20..=4096).contains(&token.len())
                && token
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'_' | b'-'))
        }
        _ => false,
    }
}

/// Remember this phone for the person, or refresh it.
///
/// Keyed by token: the same phone registering again refreshes its row, and a phone someone else
/// signed in on is handed to the caller rather than waking two accounts.
#[utoipa::path(
    put,
    path = "/api/v1/me/push/devices",
    tag = "push",
    request_body = DeviceRequest,
    responses(
        (status = 204, description = "Phone remembered"),
        (status = 400, description = "Not a device token (`reason`: `token`)"),
        (status = 409, description = "The instance has no relay (`reason`: `no_relay`)")
    )
)]
pub async fn register(
    State(state): State<AppState>,
    person: Person,
    Json(body): Json<DeviceRequest>,
) -> Result<StatusCode, Problem> {
    if state.config.push_relay_url.is_none() {
        return Err(conflict("no_relay"));
    }
    if !plausible(&body.platform, &body.token) {
        return Err(Problem::bad_request().with_reason("token"));
    }
    state
        .db
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "INSERT INTO push_devices (id, account_id, platform, token) VALUES ($1, $2, $3, $4) \
             ON CONFLICT (token) DO UPDATE SET account_id = EXCLUDED.account_id, \
             platform = EXCLUDED.platform, last_seen_at = now()",
            [
                Uuid::new_v4().into(),
                person.account.into(),
                body.platform.into(),
                body.token.into(),
            ],
        ))
        .await
        .map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Forget this phone (the app signing out). Idempotent, and only ever the person's own.
#[utoipa::path(
    delete,
    path = "/api/v1/me/push/devices",
    tag = "push",
    request_body = DeviceToken,
    responses((status = 204, description = "Forgotten (or was not known)"))
)]
pub async fn unregister(
    State(state): State<AppState>,
    person: Person,
    Json(body): Json<DeviceToken>,
) -> Result<StatusCode, Problem> {
    state
        .db
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "DELETE FROM push_devices WHERE token = $1 AND account_id = $2",
            [body.token.into(), person.account.into()],
        ))
        .await
        .map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Forget every phone of an account, when all its sessions end (signing out everywhere): a phone
/// that is signed out must not keep being woken.
pub async fn forget_account(db: &impl ConnectionTrait, account: Uuid) {
    if let Err(error) = db
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "DELETE FROM push_devices WHERE account_id = $1",
            [account.into()],
        ))
        .await
    {
        tracing::warn!(%error, "could not forget an account's phones");
    }
}

/// The phones of an account worth waking, forgetting those unseen for too long.
pub async fn devices(
    db: &impl ConnectionTrait,
    account: Uuid,
    now: OffsetDateTime,
) -> Result<Vec<Device>, DbErr> {
    let floor = now - time::Duration::days(STALE_DAYS);
    db.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "DELETE FROM push_devices WHERE account_id = $1 AND last_seen_at < $2",
        [account.into(), floor.into()],
    ))
    .await?;
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT id, platform, token FROM push_devices WHERE account_id = $1",
            [account.into()],
        ))
        .await?;
    rows.iter()
        .map(|row| {
            Ok(Device {
                id: row.try_get("", "id")?,
                platform: row.try_get("", "platform")?,
                token: row.try_get("", "token")?,
            })
        })
        .collect()
}

/// What a phone test reached.
#[derive(Debug, Serialize, ToSchema)]
pub struct PhoneTest {
    /// How many of the person's phones were woken.
    pub devices: usize,
}

/// Wake each of the person's phones with the test notification.
///
/// The phones' counterpart of [`super::web::test`], through the same chain as a real entry: the
/// relay, the phone waking up, and the phone asking `GET /api/v1/me/push/pending`, which hands it the
/// test once. Quiet hours and settings do not apply: it was asked for.
#[utoipa::path(
    post,
    path = "/api/v1/me/push/devices/test",
    operation_id = "test_device_push",
    tag = "push",
    responses(
        (status = 200, description = "Woken", body = PhoneTest),
        (status = 409, description = "Refused (`reason`: `no_relay`, `no_phone`, `test_just_sent`)")
    )
)]
pub async fn test(
    State(state): State<AppState>,
    person: Person,
) -> Result<Json<PhoneTest>, Problem> {
    if state.config.push_relay_url.is_none() {
        return Err(conflict("no_relay"));
    }
    let phones = devices(&state.db, person.account, OffsetDateTime::now_utc())
        .await
        .map_err(internal)?;
    if phones.is_empty() {
        return Err(conflict("no_phone"));
    }
    super::web::arm_test(&state, person.account).await?;
    let count = phones.len();
    for phone in phones {
        wake(&state, phone).await;
    }
    Ok(Json(PhoneTest { devices: count }))
}

/// Wake one phone through the relay; a phone the vendor says is gone is forgotten.
pub async fn wake(state: &AppState, device: Device) {
    ask_relay(state, device, "/v1/send").await;
}

/// How long the phones' sync waits after a read, so a burst of reading (one, then another, then
/// "mark all read") costs one silent push, sent once the burst is over.
const SYNC_WINDOW_SECS: u64 = 3;

/// Some of the person's entries were read: in a few seconds, tell each of their phones, silently, to
/// count again and clear the read ones from the screen (ADR 0002). Reads within the window ride on
/// the push already planned. Nothing is sent when the instance has no relay.
pub async fn sync_soon(state: &AppState, account: Uuid) {
    if state.config.push_relay_url.is_none() {
        return;
    }
    let planned: Result<Option<String>, _> = state
        .valkey
        .set(
            format!("push:sync:{account}"),
            "1",
            Some(Expiration::EX(SYNC_WINDOW_SECS as i64)),
            Some(SetOptions::NX),
            false,
        )
        .await;
    // Already planned for this burst, or Valkey is away (the count catches up at the next push).
    if !matches!(planned, Ok(Some(_))) {
        return;
    }
    let state = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(SYNC_WINDOW_SECS)).await;
        let phones = match devices(&state.db, account, OffsetDateTime::now_utc()).await {
            Ok(phones) => phones,
            Err(error) => {
                tracing::warn!(%error, "could not list the phones to sync");
                return;
            }
        };
        for phone in phones {
            ask_relay(&state, phone, "/v1/sync").await;
        }
    });
}

/// Post a device to one of the relay's routes; a phone the vendor says is gone is forgotten.
async fn ask_relay(state: &AppState, device: Device, route: &'static str) {
    let Some(relay) = &state.config.push_relay_url else {
        return;
    };
    let sent = state
        .push
        .http
        .post(format!("{relay}{route}"))
        .json(&serde_json::json!({ "platform": device.platform, "token": device.token }))
        .timeout(Duration::from_secs(15))
        .send()
        .await;
    match sent.map(|response| response.status().as_u16()) {
        Ok(200..=299) => {}
        Ok(410) => {
            tracing::info!(device = %device.id, "phone gone; forgotten");
            if let Err(error) = state
                .db
                .execute_raw(Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    "DELETE FROM push_devices WHERE id = $1",
                    [device.id.into()],
                ))
                .await
            {
                tracing::warn!(%error, "could not forget a phone");
            }
        }
        // A relay older than the sync push: the count catches up at the next notification.
        Ok(404) if route == "/v1/sync" => {
            tracing::debug!("the push relay does not sync yet");
        }
        Ok(status) => {
            tracing::warn!(status, device = %device.id, route, "the push relay refused")
        }
        Err(error) => {
            tracing::warn!(error = %error.without_url(), "could not reach the push relay")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::plausible;

    #[test]
    fn device_tokens_are_checked_per_platform() {
        assert!(plausible("ios", &"ab12".repeat(16)));
        assert!(!plausible("ios", "not-hex-at-all-not-hex-at-all-not-hex"));
        assert!(plausible(
            "android",
            "dQw4w9WgXcQ:APA91bHPRgkF3JUikC4ENAHEeMrd41Zxv3hVZjC9KtT8"
        ));
        assert!(!plausible("android", "short"));
        assert!(!plausible("windows", &"ab12".repeat(16)));
    }
}

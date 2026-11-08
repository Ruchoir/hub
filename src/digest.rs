//! The mail fallback: what is still unread after a while goes out as one digest per person.
//!
//! The one channel that reaches someone with no Ruchoir page open and no device subscribed, and the
//! one that goes through no service the instance did not choose: its own mail relay. It is meant as
//! a catch-up, not as a second notification stream, so:
//!
//! - it waits (`HUB_DIGEST_DELAY_SECS`, fifteen minutes by default), which leaves the time to read
//!   the entry in the app or on a phone first;
//! - it skips anyone who is connected right now: they have the app open, and the app has already
//!   told them;
//! - it sends one message per person per sweep, listing what is waiting, rather than one per entry;
//! - it follows the same settings as everything else ([`crate::prefs::allows`]), plus its own
//!   switch, and holds back during quiet hours and "do not disturb" instead of dropping the entries:
//!   they go out once the window closes, if still unread.
//!
//! The hub words the lines in the six languages ([`crate::push::words`]) and asks Propolis to send
//! them (decision of 2026-11-08): Propolis picks the person's language and address, which never
//! leave it.
//!
//! Every entry is decided exactly once (`inbox_entries.mail_handled_at`), and the entries being
//! decided are locked with `SKIP LOCKED`, so two hub processes sweeping at the same moment never
//! mail the same entry twice.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use sea_orm::{
    ConnectionTrait, DatabaseTransaction, DbBackend, DbErr, Statement, TransactionTrait,
};
use serde_json::{json, Map, Value};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::push::words::{self, Locale};
use crate::AppState;

/// The scope the hub asks Propolis's mail with.
pub const SCOPE: &str = "propolis:mail.digest";
/// How often the sweep runs.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);
/// Past this age an entry is never mailed: a day-old mention belongs to the inbox.
const MAX_AGE_SECS: i64 = 24 * 60 * 60;
/// How many entries one sweep considers. The rest wait for the next one, a minute later.
const BATCH: i64 = 500;
/// How many lines a digest lists before Propolis sums up the rest.
pub const LISTED: usize = 8;

/// Start the periodic sweep, when the hub can reach Propolis and the digest is not turned off.
pub fn spawn(state: AppState) {
    if state.config.digest_delay_secs == 0 {
        tracing::info!("HUB_DIGEST_DELAY_SECS is 0: the mail digest is off");
        return;
    }
    if state.propolis.is_none() {
        tracing::warn!("no HUB_CLIENT_KEY: the mail digest is off");
        return;
    }
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(SWEEP_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            match sweep(&state, OffsetDateTime::now_utc()).await {
                Ok(0) => {}
                Ok(sent) => tracing::info!(sent, "sent unread digests"),
                Err(error) => tracing::warn!(%error, "unread digest sweep failed"),
            }
        }
    });
}

/// An unread entry waiting for its decision.
struct Waiting {
    id: Uuid,
    category: String,
    organisation: Option<Uuid>,
    link: String,
    variables: Map<String, Value>,
}

/// Mark entries as decided, whatever the decision was.
async fn mark_handled(
    txn: &DatabaseTransaction,
    ids: Vec<Uuid>,
    now: OffsetDateTime,
) -> Result<(), DbErr> {
    if ids.is_empty() {
        return Ok(());
    }
    txn.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "UPDATE inbox_entries SET mail_handled_at = $1 WHERE id = ANY($2)",
        [now.into(), ids.into()],
    ))
    .await?;
    Ok(())
}

/// The digest Propolis is asked to send: each line worded in every language.
pub fn request(account: Uuid, wanted: &[(&str, &Map<String, Value>)], link: &str) -> Value {
    let items: Vec<Value> = wanted
        .iter()
        .take(LISTED)
        .map(|(category, variables)| {
            let mut headline = Map::new();
            let mut excerpt = String::new();
            for locale in Locale::ALL {
                let (line, detail) = words::digest_line(locale, category, variables);
                headline.insert(locale.as_str().to_owned(), Value::String(line));
                excerpt = detail;
            }
            json!({ "headline": headline, "excerpt": excerpt })
        })
        .collect();
    json!({ "account": account, "total": wanted.len(), "link": link, "items": items })
}

/// What Propolis answered about one digest.
enum Answer {
    Sent,
    /// No such active account, or no relay: nothing will change by trying again.
    Settled,
    /// Try again at the next sweep.
    Later,
}

async fn ask_propolis(state: &AppState, token: &str, body: &Value) -> Answer {
    let Some(propolis) = &state.propolis else {
        return Answer::Later;
    };
    let sent = propolis
        .http()
        .post(format!("{}/api/v1/mail/digest", propolis.issuer()))
        .bearer_auth(token)
        .json(body)
        .send()
        .await;
    match sent.map(|response| response.status().as_u16()) {
        Ok(204) => Answer::Sent,
        Ok(404) => Answer::Settled,
        // No relay: the digest is a catch-up, and the inbox keeps every entry.
        Ok(503) => {
            tracing::info!("Propolis cannot mail now: this digest is dropped");
            Answer::Settled
        }
        Ok(status) => {
            tracing::warn!(status, "Propolis refused a digest");
            Answer::Later
        }
        Err(error) => {
            tracing::warn!(error = %error.without_url(), "could not reach Propolis for a digest");
            Answer::Later
        }
    }
}

/// Run one sweep as of `now`, returning how many digests were sent.
pub async fn sweep(state: &AppState, now: OffsetDateTime) -> Result<usize, String> {
    let fail = |e: DbErr| e.to_string();
    let delay = time::Duration::seconds(state.config.digest_delay_secs.max(0));
    let too_old = now - time::Duration::seconds(MAX_AGE_SECS);
    // What can no longer be mailed is settled first, in bulk: read in the meantime, or too old.
    // That keeps the pending index the size of the real backlog.
    state
        .db
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE inbox_entries SET mail_handled_at = $1 \
             WHERE mail_handled_at IS NULL AND (read_at IS NOT NULL OR created_at < $2)",
            [now.into(), too_old.into()],
        ))
        .await
        .map_err(fail)?;

    let txn = state.db.begin().await.map_err(fail)?;
    let rows = txn
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT id, account_id, category, organisation_id, link, variables FROM inbox_entries \
             WHERE mail_handled_at IS NULL AND read_at IS NULL AND created_at <= $1 \
             ORDER BY created_at LIMIT $2 \
             FOR UPDATE SKIP LOCKED",
            [(now - delay).into(), BATCH.into()],
        ))
        .await
        .map_err(fail)?;
    // By account so a run is deterministic, and newest first within one person's digest.
    let mut by_account: BTreeMap<Uuid, Vec<Waiting>> = BTreeMap::new();
    for row in rows {
        let account: Uuid = row.try_get("", "account_id").map_err(fail)?;
        let variables: Value = row.try_get("", "variables").map_err(fail)?;
        by_account.entry(account).or_default().push(Waiting {
            id: row.try_get("", "id").map_err(fail)?,
            category: row.try_get("", "category").map_err(fail)?,
            organisation: row.try_get("", "organisation_id").map_err(fail)?,
            link: row.try_get("", "link").map_err(fail)?,
            variables: crate::push::web::variables_of(variables),
        });
    }

    let mut token: Option<String> = None;
    let mut sent = 0;
    for (account, mut waiting) in by_account {
        waiting.reverse();
        let ids: Vec<Uuid> = waiting.iter().map(|w| w.id).collect();
        let prefs = crate::prefs::load(&txn, account).await.map_err(fail)?;
        if !prefs.enabled || !prefs.email {
            mark_handled(&txn, ids, now).await.map_err(fail)?;
            continue;
        }
        let choice = crate::live::presence::choice_of(&txn, account)
            .await
            .map_err(fail)?;
        // Not now, but not never: left undecided, they go out once the window closes.
        if !crate::prefs::may_interrupt(&prefs, choice.as_deref(), now) {
            continue;
        }
        // Connected somewhere: the app has shown them already, and a mail on top would be noise.
        if crate::live::presence::is_online(&state.valkey, account).await {
            mark_handled(&txn, ids, now).await.map_err(fail)?;
            continue;
        }
        let mut levels: HashMap<Uuid, Option<String>> = HashMap::new();
        let mut wanted: Vec<&Waiting> = Vec::new();
        for entry in &waiting {
            let level = match entry.organisation {
                Some(organisation) => match levels.get(&organisation) {
                    Some(level) => level.clone(),
                    None => {
                        let level = crate::prefs::level(&txn, account, organisation)
                            .await
                            .map_err(fail)?;
                        levels.insert(organisation, level.clone());
                        level
                    }
                },
                None => None,
            };
            if crate::prefs::allows(
                &entry.category,
                &prefs,
                level.as_deref(),
                crate::prefs::Delivery::Email,
            ) {
                wanted.push(entry);
            }
        }
        let Some(newest) = wanted.first() else {
            mark_handled(&txn, ids, now).await.map_err(fail)?;
            continue;
        };
        let lines: Vec<(&str, &Map<String, Value>)> = wanted
            .iter()
            .map(|w| (w.category.as_str(), &w.variables))
            .collect();
        let body = request(account, &lines, &newest.link);
        if token.is_none() {
            let Some(propolis) = &state.propolis else {
                break;
            };
            token = Some(propolis.token(SCOPE).await?);
        }
        match ask_propolis(state, token.as_deref().unwrap_or_default(), &body).await {
            Answer::Sent => {
                mark_handled(&txn, ids, now).await.map_err(fail)?;
                sent += 1;
            }
            Answer::Settled => mark_handled(&txn, ids, now).await.map_err(fail)?,
            // Left undecided: the next sweep tries again, until the entries age out of the window.
            Answer::Later => {}
        }
    }
    txn.commit().await.map_err(fail)?;
    Ok(sent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_digest_is_worded_in_every_language_and_lists_eight_lines() {
        let variables =
            json!({ "actor_name": "Camille", "place": "#général", "excerpt": "Jeudi ?" });
        let variables = variables.as_object().unwrap();
        let lines: Vec<(&str, &Map<String, Value>)> =
            (0..10).map(|_| ("mention", variables)).collect();
        let body = request(Uuid::nil(), &lines, "https://chat.example.org/");
        assert_eq!(body["total"], 10);
        assert_eq!(body["link"], "https://chat.example.org/");
        let items = body["items"].as_array().unwrap();
        assert_eq!(items.len(), LISTED);
        assert_eq!(
            items[0]["headline"]["fr"],
            "Camille vous a mentionné · #général"
        );
        assert_eq!(
            items[0]["headline"]["pl"],
            "Camille wspomniał o Tobie · #général"
        );
        assert_eq!(items[0]["headline"].as_object().unwrap().len(), 6);
        assert_eq!(items[0]["excerpt"], "Jeudi ?");
    }
}

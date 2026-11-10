//! Presence: the live heartbeat, the presence a person chose, and who sees it (design
//! `docs/design/hub.md`, section 5).
//!
//! - **Live heartbeat**: while a person holds a socket or a stream, it refreshes a Valkey key
//!   `presence:<account>` with a short lifetime. A present key means reachable now.
//! - **Connections counted across processes** (`presence_conns:<account>`): the monolith cleared the
//!   heartbeat when a process lost its last connection of a person, even when another process still
//!   held one. Now a person goes offline only when their last connection anywhere goes.
//! - **Manual choice** (`active`, `away`, `dnd`, `invisible`, or automatic): how a reachable person
//!   reads to others, kept by the hub (it moved from Propolis's accounts).
//! - **Who sees it**: whoever shares an organisation, from the members' replica. A change (online,
//!   offline, a new choice) is pushed to them as `hub.presence.changed`.
//! - **A product's question** (`POST /api/v1/presence/online`): which of the accounts it names are
//!   online as others see them, for the chat's `@ici`. The product computed who may be asked about;
//!   the answer says nothing more than a presence dot would.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use fred::interfaces::KeysInterface;
use fred::prelude::Pool;
use fred::types::Expiration;
use ruchoir_events::ClientFrame;
use ruchoir_http::Problem;
use sea_orm::{ConnectionTrait, DbBackend, DbErr, Statement};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use super::ws::HEARTBEAT_SECS;
use crate::auth::{Person, Publisher};
use crate::{replica, AppState};

const PREFIX: &str = "presence:";
const CONNS_PREFIX: &str = "presence_conns:";

/// The manual choices a person may make.
pub const CHOICES: [&str; 4] = ["active", "away", "dnd", "invisible"];

/// How long a heartbeat holds: two of them, so one late ping does not read as gone.
pub const TTL_SECS: i64 = 2 * HEARTBEAT_SECS as i64;

/// Refresh an account's heartbeat, and the count of its connections with it.
pub async fn heartbeat(valkey: &Pool, account: Uuid) {
    if let Err(error) = valkey
        .set::<(), _, _>(
            format!("{PREFIX}{account}"),
            "1",
            Some(Expiration::EX(TTL_SECS)),
            None,
            false,
        )
        .await
    {
        tracing::warn!(%error, "cannot write a presence heartbeat");
    }
    // A count nothing refreshes lapses with its heartbeat: a process that died leaves no ghost.
    let _ = valkey
        .expire::<(), _>(format!("{CONNS_PREFIX}{account}"), TTL_SECS, None)
        .await;
}

/// Clear an account's heartbeat.
pub async fn clear(valkey: &Pool, account: Uuid) {
    let _ = valkey
        .del::<u64, _>(vec![
            format!("{PREFIX}{account}"),
            format!("{CONNS_PREFIX}{account}"),
        ])
        .await;
}

/// Whether an account has a live heartbeat.
pub async fn is_online(valkey: &Pool, account: Uuid) -> bool {
    matches!(valkey.exists::<u64, _>(format!("{PREFIX}{account}")).await, Ok(n) if n > 0)
}

/// A connection of `account` opened, on any process: online, and announced when it was the first.
pub async fn arrived(state: &AppState, account: Uuid) {
    let count: i64 = state
        .valkey
        .incr(format!("{CONNS_PREFIX}{account}"))
        .await
        .unwrap_or(1);
    heartbeat(&state.valkey, account).await;
    if count == 1 {
        announce(state, account).await;
    }
}

/// A connection of `account` closed: offline, and announced, when it was the last one anywhere.
pub async fn departed(state: &AppState, account: Uuid) {
    let count: i64 = state
        .valkey
        .decr(format!("{CONNS_PREFIX}{account}"))
        .await
        .unwrap_or(0);
    if count <= 0 {
        clear(&state.valkey, account).await;
        announce(state, account).await;
    }
}

pub async fn choice_of(db: &impl ConnectionTrait, account: Uuid) -> Result<Option<String>, DbErr> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT choice FROM presence_choices WHERE account_id = $1",
            [account.into()],
        ))
        .await?;
    row.map(|r| r.try_get("", "choice")).transpose()
}

/// The most accounts one presence question may name.
pub const MAX_ASKED: usize = 5_000;

/// Which of `accounts` are online as others see them, in the order asked, each once.
///
/// Online means [`effective_for_others`] is not `offline`: a live heartbeat, and any choice but
/// `invisible`. `away` and `dnd` stay online, as they are reachable and their dot says so; what
/// they let through is their notification preferences' business, not presence's. This is the
/// monolith's `@ici` (`presence::is_online`, the heartbeat alone) with one correction: it counted a
/// person who chose `invisible`, which told the room they were there.
///
/// Two reads whatever the number asked: one `MGET` of the heartbeats, then one query of the
/// choices of those with one.
pub async fn online_among(
    db: &impl ConnectionTrait,
    valkey: &Pool,
    accounts: &[Uuid],
) -> Result<Vec<Uuid>, OnlineError> {
    let mut seen = std::collections::HashSet::with_capacity(accounts.len());
    let asked: Vec<Uuid> = accounts
        .iter()
        .copied()
        .filter(|a| seen.insert(*a))
        .collect();
    if asked.is_empty() {
        return Ok(Vec::new());
    }
    let keys: Vec<String> = asked.iter().map(|a| format!("{PREFIX}{a}")).collect();
    let beats: Vec<Option<String>> = valkey.mget(keys).await.map_err(OnlineError::Valkey)?;
    let live: Vec<Uuid> = asked
        .into_iter()
        .zip(beats)
        .filter_map(|(account, beat)| beat.map(|_| account))
        .collect();
    if live.is_empty() {
        return Ok(live);
    }
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT account_id, choice FROM presence_choices WHERE account_id = ANY($1)",
            [live.clone().into()],
        ))
        .await
        .map_err(OnlineError::Db)?;
    let mut choices = std::collections::HashMap::with_capacity(rows.len());
    for row in rows {
        let account: Uuid = row.try_get("", "account_id").map_err(OnlineError::Db)?;
        let choice: String = row.try_get("", "choice").map_err(OnlineError::Db)?;
        choices.insert(account, choice);
    }
    Ok(live
        .into_iter()
        .filter(|a| effective_for_others(choices.get(a).map(String::as_str), true) != "offline")
        .collect())
}

/// Why [`online_among`] could not answer.
#[derive(Debug)]
pub enum OnlineError {
    Valkey(fred::error::Error),
    Db(DbErr),
}

impl std::fmt::Display for OnlineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OnlineError::Valkey(error) => write!(f, "valkey: {error}"),
            OnlineError::Db(error) => write!(f, "database: {error}"),
        }
    }
}

/// The accounts a product asks about.
#[derive(Debug, Deserialize, ToSchema)]
pub struct Asked {
    /// At most 5,000 accounts; repeats are answered once.
    pub accounts: Vec<Uuid>,
}

/// Those of the asked accounts who are online.
#[derive(Debug, Serialize, ToSchema)]
pub struct Online {
    /// In the order asked. Online: a live connection and any choice but `invisible` (`away` and
    /// `dnd` count).
    pub online: Vec<Uuid>,
}

/// Which of these accounts are online as others see them, for a product (the chat's `@ici`).
#[utoipa::path(
    post,
    path = "/api/v1/presence/online",
    tag = "live",
    request_body = Asked,
    responses(
        (status = 200, description = "Those online: connected now, and not `invisible` (`away` and `dnd` count)", body = Online),
        (status = 400, description = "Refused (`reason`: `accounts`, more than 5,000)"),
        (status = 401, description = "No valid token"),
        (status = 403, description = "Not a product with `hub:publish`")
    )
)]
pub async fn online(
    State(state): State<AppState>,
    _publisher: Publisher,
    Json(body): Json<Asked>,
) -> Result<Json<Online>, Problem> {
    if body.accounts.len() > MAX_ASKED {
        return Err(Problem::bad_request().with_reason("accounts"));
    }
    let online = online_among(&state.db, &state.valkey, &body.accounts)
        .await
        .map_err(|error| {
            tracing::warn!(%error, "cannot read who is online");
            Problem::internal()
        })?;
    Ok(Json(Online { online }))
}

/// What others see of `account` now.
pub async fn seen_by_others(state: &AppState, account: Uuid) -> &'static str {
    let choice = choice_of(&state.db, account).await.ok().flatten();
    effective_for_others(choice.as_deref(), is_online(&state.valkey, account).await)
}

/// Tell everyone who shares an organisation with `account` how it reads now.
pub async fn announce(state: &AppState, account: Uuid) {
    let audience = match replica::co_members(&state.db, account).await {
        Ok(audience) => audience,
        Err(error) => {
            tracing::warn!(%error, "cannot read who shares an organisation");
            return;
        }
    };
    let presence = seen_by_others(state, account).await;
    if let Ok(frame) = ClientFrame::new(
        "hub.presence.changed",
        serde_json::json!({ "account": account, "presence": presence }),
    ) {
        state.fanout.publish(audience, frame).await;
    }
}

/// One member's presence.
#[derive(Debug, Serialize, ToSchema)]
pub struct MemberPresence {
    pub account: Uuid,
    /// `active`, `away`, `dnd` or `offline`.
    pub presence: String,
}

/// The presence of an organisation's members, for one of them.
#[utoipa::path(
    get,
    path = "/api/v1/organisations/{id}/presence",
    tag = "live",
    params(("id" = Uuid, Path, description = "The organisation")),
    responses(
        (status = 200, description = "Every member's presence", body = [MemberPresence]),
        (status = 404, description = "Not one of the reader's organisations")
    )
)]
pub async fn organisation(
    State(state): State<AppState>,
    person: Person,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<MemberPresence>>, Problem> {
    let internal = |_| Problem::internal();
    if !replica::is_member(&state.db, id, person.account)
        .await
        .map_err(internal)?
    {
        return Err(Problem::not_found());
    }
    let rows = state
        .db
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT m.account_id, c.choice FROM organisation_members m              LEFT JOIN presence_choices c ON c.account_id = m.account_id              WHERE m.organisation_id = $1 ORDER BY m.account_id",
            [id.into()],
        ))
        .await
        .map_err(internal)?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let account: Uuid = row.try_get("", "account_id").map_err(internal)?;
        let choice: Option<String> = row.try_get("", "choice").map_err(internal)?;
        let online = is_online(&state.valkey, account).await;
        out.push(MemberPresence {
            account,
            presence: effective_for_others(choice.as_deref(), online).to_owned(),
        });
    }
    Ok(Json(out))
}

/// The person's own presence, as they see it.
#[derive(Debug, Serialize, ToSchema)]
pub struct MyPresence {
    /// What they chose, or `null` for automatic.
    pub choice: Option<String>,
    /// How it reads to them (`invisible` stays visible to its author).
    pub presence: String,
}

/// Read one's own presence.
#[utoipa::path(
    get,
    path = "/api/v1/me/presence",
    tag = "live",
    responses((status = 200, description = "The person's presence", body = MyPresence))
)]
pub async fn mine(
    State(state): State<AppState>,
    person: Person,
) -> Result<Json<MyPresence>, Problem> {
    let choice = choice_of(&state.db, person.account)
        .await
        .map_err(|_| Problem::internal())?;
    let online = is_online(&state.valkey, person.account).await;
    Ok(Json(MyPresence {
        presence: effective_for_self(choice.as_deref(), online).to_owned(),
        choice,
    }))
}

/// A presence to choose.
#[derive(Debug, Deserialize, ToSchema)]
pub struct Choice {
    /// `active`, `away`, `dnd`, `invisible`, or `null` for automatic.
    pub choice: Option<String>,
}

/// Choose one's presence; everyone who shares an organisation sees the change.
#[utoipa::path(
    put,
    path = "/api/v1/me/presence",
    tag = "live",
    request_body = Choice,
    responses(
        (status = 204, description = "Chosen"),
        (status = 400, description = "Not a presence one may choose")
    )
)]
pub async fn choose(
    State(state): State<AppState>,
    person: Person,
    Json(body): Json<Choice>,
) -> Result<StatusCode, Problem> {
    let statement = match body.choice.as_deref() {
        None => Statement::from_sql_and_values(
            DbBackend::Postgres,
            "DELETE FROM presence_choices WHERE account_id = $1",
            [person.account.into()],
        ),
        Some(choice) if CHOICES.contains(&choice) => Statement::from_sql_and_values(
            DbBackend::Postgres,
            "INSERT INTO presence_choices (account_id, choice) VALUES ($1, $2)              ON CONFLICT (account_id) DO UPDATE SET choice = $2, updated_at = now()",
            [person.account.into(), choice.into()],
        ),
        Some(_) => return Err(Problem::bad_request().with_reason("choice")),
    };
    state
        .db
        .execute_raw(statement)
        .await
        .map_err(|_| Problem::internal())?;
    announce(&state, person.account).await;
    Ok(StatusCode::NO_CONTENT)
}

/// The presence a *third party* sees. `invisible` and an absent heartbeat both read as `offline`.
///
/// An override says how to read someone who is reachable; it does not make them reachable. Without
/// a heartbeat the answer is `offline` whatever they chose, because the alternative is a dot that
/// claims a colleague is there when nothing of theirs is connected. `active` is the case that made
/// this visible: it was returned even with the heartbeat long lapsed, so anyone who had once
/// picked "online" stayed green to the whole space for good.
pub fn effective_for_others(manual: Option<&str>, online: bool) -> &'static str {
    if !online {
        return "offline";
    }
    match manual {
        Some("invisible") => "offline",
        Some("dnd") => "dnd",
        Some("away") => "away",
        _ => "active",
    }
}

/// The presence a user sees for *themselves* (keeps `invisible` visible in their own UI).
///
/// Their own choice is shown back to them even while nothing is connected, so the menu reflects
/// what they picked rather than silently disagreeing with it. What everyone else sees is still
/// [`effective_for_others`], which needs a heartbeat.
pub fn effective_for_self(manual: Option<&str>, online: bool) -> &'static str {
    match manual {
        Some("invisible") => "invisible",
        Some("dnd") => "dnd",
        Some("away") => "away",
        Some("active") => "active",
        _ => {
            if online {
                "active"
            } else {
                "offline"
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{effective_for_others, effective_for_self};

    #[test]
    fn auto_presence_follows_the_heartbeat() {
        assert_eq!(effective_for_others(None, true), "active");
        assert_eq!(effective_for_others(None, false), "offline");
    }

    #[test]
    fn invisible_reads_as_offline_to_others_but_visible_to_self() {
        assert_eq!(effective_for_others(Some("invisible"), true), "offline");
        assert_eq!(effective_for_self(Some("invisible"), true), "invisible");
    }

    #[test]
    fn an_override_colours_a_live_connection() {
        assert_eq!(effective_for_others(Some("dnd"), true), "dnd");
        assert_eq!(effective_for_others(Some("away"), true), "away");
        assert_eq!(effective_for_others(Some("active"), true), "active");
    }

    #[test]
    fn no_override_makes_an_absent_user_look_present() {
        // The rule that matters: a choice says how to read someone who is reachable, it does not
        // make them reachable. Anything else leaves a colleague green with nothing connected.
        for manual in [
            None,
            Some("active"),
            Some("away"),
            Some("dnd"),
            Some("invisible"),
        ] {
            assert_eq!(effective_for_others(manual, false), "offline");
        }
    }

    #[test]
    fn a_user_still_sees_their_own_choice_while_disconnected() {
        assert_eq!(effective_for_self(Some("away"), false), "away");
        assert_eq!(effective_for_self(None, false), "offline");
    }
}

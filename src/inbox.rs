//! The inbox: what a person was told about, by every product (design `docs/design/hub.md`, sections 2
//! and 3).
//!
//! From the monolith's notifications, minus what only the chat knows (who a message notifies, a
//! reaction, an edit): the hub cannot read a product's tables, so a product publishes everything an
//! entry shows, and the hub keeps it as it was published.
//!
//! - **Publishing** (`POST /api/v1/inbox`, a product with `hub:publish`): one entry per recipient the
//!   product computed. Its `kind` is the product's own (`chat.mention` from the chat), its `category`
//!   one of the few the hub knows, which decide urgency, preferences and mail.
//! - **Reading** (`GET /api/v1/me/inbox`, a person): newest first, by pages, with what is unread.
//! - **Marking read**, one entry or all of them (of one organisation, when asked). Idempotent.
//!
//! Who an entry is for is the product's decision; the hub never widens or narrows it (ADR 0003).

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use ruchoir_http::Problem;
use sea_orm::{ConnectionTrait, DbBackend, DbErr, QueryResult, Statement};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::auth::{Person, Publisher};
use crate::AppState;
use ruchoir_events::ClientFrame;

const DEFAULT_LIMIT: u64 = 30;
const MAX_LIMIT: u64 = 100;
/// The most recipients one publication may name.
pub const MAX_RECIPIENTS: usize = 10_000;
/// The largest `variables` object, serialised.
pub const MAX_VARIABLES_BYTES: usize = 4096;

/// The categories the hub knows: what decides urgency, preferences and mail.
pub const CATEGORIES: &[&str] = &[
    "mention",
    "reply",
    "direct",
    "activity",
    "reminder",
    "invitation",
    "notice",
];

fn internal(error: DbErr) -> Problem {
    tracing::error!(%error, "inbox store");
    Problem::internal()
}

/// What a product publishes to the inbox.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Publication {
    /// `<product>.<name>`, the product being the publisher (`chat.mention`).
    pub kind: String,
    /// One of `mention`, `reply`, `direct`, `activity`, `reminder`, `invitation`, `notice`.
    pub category: String,
    /// The organisation it happened in, if any.
    pub organisation: Option<Uuid>,
    /// Who did it, if anyone.
    pub actor: Option<Uuid>,
    /// The product's object, an id only the product reads.
    pub object: String,
    /// Where the entry leads: an `https` address of the product.
    pub link: String,
    /// What the entry shows, as it stood: names and an excerpt (`{"place": "#général"}`).
    #[serde(default)]
    pub variables: serde_json::Map<String, Value>,
    /// The accounts it is for, as the product computed them.
    pub recipients: Vec<Uuid>,
}

/// How many entries a publication made.
#[derive(Debug, Serialize, ToSchema)]
pub struct Published {
    pub entries: usize,
}

/// Why a publication is refused, as the `reason` of a `400`.
fn refuse(reason: &'static str) -> Problem {
    Problem::bad_request().with_reason(reason)
}

impl Publication {
    /// Check a publication from `product`: its recipients without repeats, or the reason it is refused.
    fn check(&self, product: &str) -> Result<Vec<Uuid>, &'static str> {
        let (prefix, name) = self.kind.split_once('.').ok_or("kind")?;
        // A product publishes under its own name, never another's.
        if prefix != product || name.is_empty() {
            return Err("kind_not_yours");
        }
        let shaped = self
            .kind
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.');
        if !shaped || self.kind.len() > 100 {
            return Err("kind");
        }
        if !CATEGORIES.contains(&self.category.as_str()) {
            return Err("category");
        }
        if self.object.is_empty() || self.object.len() > 200 {
            return Err("object");
        }
        if !self.link.starts_with("https://") || self.link.len() > 2000 {
            return Err("link");
        }
        if serde_json::to_vec(&self.variables).map_or(true, |v| v.len() > MAX_VARIABLES_BYTES) {
            return Err("variables");
        }
        let mut recipients = self.recipients.clone();
        recipients.sort_unstable();
        recipients.dedup();
        if recipients.is_empty() || recipients.len() > MAX_RECIPIENTS {
            return Err("recipients");
        }
        Ok(recipients)
    }
}

/// Publish entries to the inbox.
#[utoipa::path(
    post,
    path = "/api/v1/inbox",
    tag = "inbox",
    request_body = Publication,
    responses(
        (status = 202, description = "Stored, one entry per recipient", body = Published),
        (status = 400, description = "Refused (`reason`: `kind`, `kind_not_yours`, `category`, `object`, `link`, `variables`, `recipients`)"),
        (status = 401, description = "No valid token"),
        (status = 403, description = "Not a product with `hub:publish`")
    )
)]
pub async fn publish(
    State(state): State<AppState>,
    publisher: Publisher,
    Json(publication): Json<Publication>,
) -> Result<(StatusCode, Json<Published>), Problem> {
    let recipients = publication.check(&publisher.client_id).map_err(refuse)?;
    let variables = Value::Object(publication.variables.clone());
    let ids: Vec<Uuid> = recipients.iter().map(|_| Uuid::new_v4()).collect();
    // One statement for every recipient: a publication is stored whole or not at all.
    state
        .db
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "INSERT INTO inbox_entries (id, account_id, product, kind, category, organisation_id, actor_id, \
             object_id, link, variables) \
             SELECT id, account_id, $3, $4, $5, $6, $7, $8, $9, $10 FROM unnest($1::uuid[], $2::uuid[]) AS r(id, account_id)",
            [
                ids.into(),
                recipients.clone().into(),
                publisher.client_id.into(),
                publication.kind.clone().into(),
                publication.category.clone().into(),
                publication.organisation.into(),
                publication.actor.into(),
                publication.object.into(),
                publication.link.into(),
                variables.into(),
            ],
        ))
        .await
        .map_err(internal)?;
    // The pages and apps the recipients have open learn of it at once (`hub.inbox.new`); the entry
    // itself is read through the API, so the frame carries only what a badge needs.
    if let Ok(frame) = ClientFrame::new(
        "hub.inbox.new",
        serde_json::json!({ "kind": publication.kind, "category": publication.category, "organisation": publication.organisation }),
    ) {
        state.fanout.publish(recipients.clone(), frame).await;
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(Published {
            entries: recipients.len(),
        }),
    ))
}

/// One entry, as its reader sees it.
#[derive(Debug, Serialize, ToSchema)]
pub struct Entry {
    pub id: Uuid,
    pub product: String,
    pub kind: String,
    pub category: String,
    pub organisation: Option<Uuid>,
    pub actor: Option<Uuid>,
    pub object: String,
    pub link: String,
    #[schema(value_type = Object)]
    pub variables: Value,
    /// RFC 3339.
    pub created_at: String,
    /// RFC 3339, when read.
    pub read_at: Option<String>,
}

fn rfc3339(at: OffsetDateTime) -> String {
    at.format(&Rfc3339).unwrap_or_default()
}

impl Entry {
    fn from_row(row: &QueryResult) -> Result<Self, DbErr> {
        let created_at: OffsetDateTime = row.try_get("", "created_at")?;
        let read_at: Option<OffsetDateTime> = row.try_get("", "read_at")?;
        Ok(Self {
            id: row.try_get("", "id")?,
            product: row.try_get("", "product")?,
            kind: row.try_get("", "kind")?,
            category: row.try_get("", "category")?,
            organisation: row.try_get("", "organisation_id")?,
            actor: row.try_get("", "actor_id")?,
            object: row.try_get("", "object_id")?,
            link: row.try_get("", "link")?,
            variables: row.try_get("", "variables")?,
            created_at: rfc3339(created_at),
            read_at: read_at.map(rfc3339),
        })
    }
}

/// A page of the inbox.
#[derive(Debug, Serialize, ToSchema)]
pub struct Page {
    pub entries: Vec<Entry>,
    /// Pass as `before` for the next page; absent on the last.
    pub next_before: Option<Uuid>,
    /// What is unread in the whole inbox (of the organisation, when one was asked).
    pub unread_count: i64,
}

/// What part of the inbox to read.
#[derive(Debug, Deserialize, IntoParams)]
pub struct FeedQuery {
    /// Only what is unread.
    #[serde(default)]
    pub unread: bool,
    /// Page size, at most 100.
    pub limit: Option<u64>,
    /// Entries older than this one (keyset pagination).
    pub before: Option<Uuid>,
    /// Only this organisation's entries.
    pub organisation: Option<Uuid>,
}

/// The person's inbox, newest first, with what is unread.
#[utoipa::path(
    get,
    path = "/api/v1/me/inbox",
    tag = "inbox",
    params(FeedQuery),
    responses(
        (status = 200, description = "A page of the inbox", body = Page),
        (status = 401, description = "No valid token"),
        (status = 403, description = "Not a person's token")
    )
)]
pub async fn read(
    State(state): State<AppState>,
    person: Person,
    Query(query): Query<FeedQuery>,
) -> Result<Json<Page>, Problem> {
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    // The cursor resolves only among the reader's own entries.
    let rows = state
        .db
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT e.* FROM inbox_entries e \
             LEFT JOIN inbox_entries c ON c.id = $2 AND c.account_id = $1 \
             WHERE e.account_id = $1 \
               AND ($3::boolean = false OR e.read_at IS NULL) \
               AND ($4::uuid IS NULL OR e.organisation_id = $4) \
               AND (c.id IS NULL OR (e.created_at, e.id) < (c.created_at, c.id)) \
             ORDER BY e.created_at DESC, e.id DESC LIMIT $5",
            [
                person.account.into(),
                query.before.into(),
                query.unread.into(),
                query.organisation.into(),
                i64::try_from(limit).unwrap_or(30).into(),
            ],
        ))
        .await
        .map_err(internal)?;
    let entries = rows
        .iter()
        .map(Entry::from_row)
        .collect::<Result<Vec<_>, _>>()
        .map_err(internal)?;
    let next_before = (entries.len() as u64 == limit)
        .then(|| entries.last().map(|e| e.id))
        .flatten();
    let unread_count = unread(&state, person.account, query.organisation).await?;
    Ok(Json(Page {
        entries,
        next_before,
        unread_count,
    }))
}

async fn unread(
    state: &AppState,
    account: Uuid,
    organisation: Option<Uuid>,
) -> Result<i64, Problem> {
    let row = state
        .db
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT count(*) AS n FROM inbox_entries WHERE account_id = $1 AND read_at IS NULL \
             AND ($2::uuid IS NULL OR organisation_id = $2)",
            [account.into(), organisation.into()],
        ))
        .await
        .map_err(internal)?;
    row.map(|r| r.try_get::<i64>("", "n"))
        .transpose()
        .map_err(internal)
        .map(|n| n.unwrap_or(0))
}

/// Mark one entry read. Idempotent; another person's entry is not found.
#[utoipa::path(
    post,
    path = "/api/v1/me/inbox/{id}/read",
    tag = "inbox",
    params(("id" = Uuid, Path, description = "The entry")),
    responses(
        (status = 204, description = "Read"),
        (status = 404, description = "No such entry of the reader's")
    )
)]
pub async fn mark_read(
    State(state): State<AppState>,
    person: Person,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, Problem> {
    let found = state
        .db
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE inbox_entries SET read_at = COALESCE(read_at, now()) WHERE id = $1 AND account_id = $2 RETURNING id",
            [id.into(), person.account.into()],
        ))
        .await
        .map_err(internal)?;
    if found.is_none() {
        return Err(Problem::not_found());
    }
    announce_read(&state, person.account, vec![id], false).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Which entries to mark read at once.
#[derive(Debug, Deserialize, IntoParams)]
pub struct ReadAllQuery {
    /// Only this organisation's.
    pub organisation: Option<Uuid>,
}

/// Mark every entry read (of one organisation, when asked). Idempotent.
#[utoipa::path(
    post,
    path = "/api/v1/me/inbox/read",
    tag = "inbox",
    params(ReadAllQuery),
    responses((status = 204, description = "All read"))
)]
pub async fn mark_all_read(
    State(state): State<AppState>,
    person: Person,
    Query(query): Query<ReadAllQuery>,
) -> Result<StatusCode, Problem> {
    state
        .db
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE inbox_entries SET read_at = now() WHERE account_id = $1 AND read_at IS NULL \
             AND ($2::uuid IS NULL OR organisation_id = $2)",
            [person.account.into(), query.organisation.into()],
        ))
        .await
        .map_err(internal)?;
    announce_read(&state, person.account, Vec::new(), true).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Something was read: every page and app the person has open follows (`hub.inbox.read`), with what
/// is left unread. It never fails the request, which has done what it was asked.
async fn announce_read(state: &AppState, account: Uuid, ids: Vec<Uuid>, all: bool) {
    let unread_count = match unread(state, account, None).await {
        Ok(count) => count,
        Err(_) => return,
    };
    if let Ok(frame) = ClientFrame::new(
        "hub.inbox.read",
        serde_json::json!({ "ids": ids, "all": all, "unread_count": unread_count }),
    ) {
        state.fanout.publish(vec![account], frame).await;
    }
}

/// The inbox's routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/inbox", post(publish))
        .route("/api/v1/me/inbox", get(read))
        .route("/api/v1/me/inbox/read", post(mark_all_read))
        .route("/api/v1/me/inbox/{id}/read", post(mark_read))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn publication() -> Publication {
        Publication {
            kind: "chat.mention".into(),
            category: "mention".into(),
            organisation: None,
            actor: None,
            object: "m1".into(),
            link: "https://chat.example.org/c/1".into(),
            variables: serde_json::Map::new(),
            recipients: vec![Uuid::nil(), Uuid::nil()],
        }
    }

    #[test]
    fn a_product_publishes_under_its_own_name() {
        assert_eq!(
            publication().check("chat").unwrap(),
            vec![Uuid::nil()],
            "repeats go"
        );
        assert!(publication().check("drive").is_err());
        let refused = |change: fn(&mut Publication)| {
            let mut p = publication();
            change(&mut p);
            p.check("chat").is_err()
        };
        assert!(refused(|p| p.kind = "chat".into()));
        assert!(refused(|p| p.kind = "chat.Mention".into()));
        assert!(refused(|p| p.category = "urgent".into()));
        assert!(refused(|p| p.link = "http://chat.example.org".into()));
        assert!(refused(|p| p.object = String::new()));
        assert!(refused(|p| p.recipients.clear()));
        assert!(refused(|p| {
            p.variables
                .insert("excerpt".into(), "x".repeat(MAX_VARIABLES_BYTES).into());
        }));
    }
}

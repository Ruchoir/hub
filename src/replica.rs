//! The hub's replica of who belongs to which organisation (design `docs/design/hub.md`, section 5;
//! Propolis's design, section 8.2).
//!
//! Propolis's change feed (`/api/v1/changes`) is the source of truth: the hub reads it in order, with
//! a service token of its own (`propolis:organizations.read`), and keeps where it stopped. Propolis's
//! events, delivered through the hub itself, only say there is something new to read; a missed event
//! costs latency, never correctness, since the feed is read every minute anyway.
//!
//! The replica holds membership alone, which is what presence needs:
//!
//! - in: `organisation.created` (its creator, the owner), `member.joined`, `member.reactivated`;
//! - out: `member.left`, `member.removed`, `member.expired`, `member.deleted`;
//! - all out: `organisation.deleted`.
//!
//! Applying a change twice changes nothing, so a page read again after a failure is harmless.

use std::sync::Arc;
use std::time::Duration;

use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, DbErr, Statement, TransactionTrait};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Notify;
use uuid::Uuid;

/// The name the feed's cursor is kept under.
const FEED: &str = "propolis";
/// The scope that reads Propolis's change feed.
pub const SCOPE: &str = "propolis:organizations.read";

/// What changes membership, from one of Propolis's events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Membership {
    Joined { organisation: Uuid, account: Uuid },
    Left { organisation: Uuid, account: Uuid },
    Dissolved { organisation: Uuid },
}

fn uuid_at(value: &Value, pointer: &str) -> Option<Uuid> {
    value.pointer(pointer)?.as_str()?.parse().ok()
}

/// What an event of Propolis's means for membership, if anything.
pub fn membership(event: &Value) -> Option<Membership> {
    let kind = event["kind"].as_str()?;
    let organisation = uuid_at(event, "/organisation")?;
    let target = || uuid_at(event, "/payload/target/id");
    match kind.strip_prefix("propolis.")? {
        "organisation.created" => Some(Membership::Joined {
            organisation,
            account: uuid_at(event, "/payload/actor")?,
        }),
        "member.joined" | "member.reactivated" => Some(Membership::Joined {
            organisation,
            account: target()?,
        }),
        "member.left" | "member.removed" | "member.expired" | "member.deleted" => {
            Some(Membership::Left {
                organisation,
                account: target()?,
            })
        }
        "organisation.deleted" => Some(Membership::Dissolved { organisation }),
        _ => None,
    }
}

/// Apply one change to the replica.
pub async fn apply(db: &impl ConnectionTrait, change: &Membership) -> Result<(), DbErr> {
    let (sql, values): (&str, Vec<sea_orm::Value>) = match change {
        Membership::Joined { organisation, account } => (
            "INSERT INTO organisation_members (organisation_id, account_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
            vec![(*organisation).into(), (*account).into()],
        ),
        Membership::Left { organisation, account } => (
            "DELETE FROM organisation_members WHERE organisation_id = $1 AND account_id = $2",
            vec![(*organisation).into(), (*account).into()],
        ),
        Membership::Dissolved { organisation } => (
            "DELETE FROM organisation_members WHERE organisation_id = $1",
            vec![(*organisation).into()],
        ),
    };
    db.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        sql,
        values,
    ))
    .await?;
    Ok(())
}

/// Everyone who shares an organisation with `account`, themselves included.
pub async fn co_members(db: &impl ConnectionTrait, account: Uuid) -> Result<Vec<Uuid>, DbErr> {
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT DISTINCT m.account_id FROM organisation_members m \
             JOIN organisation_members mine ON mine.organisation_id = m.organisation_id \
             WHERE mine.account_id = $1",
            [account.into()],
        ))
        .await?;
    rows.iter().map(|r| r.try_get("", "account_id")).collect()
}

/// Whether `account` belongs to `organisation`.
pub async fn is_member(
    db: &impl ConnectionTrait,
    organisation: Uuid,
    account: Uuid,
) -> Result<bool, DbErr> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT 1 AS one FROM organisation_members WHERE organisation_id = $1 AND account_id = $2",
            [organisation.into(), account.into()],
        ))
        .await?;
    Ok(row.is_some())
}

#[derive(Deserialize)]
struct Page {
    changes: Vec<Entry>,
    next: i64,
}

#[derive(Deserialize)]
struct Entry {
    event: Value,
}

/// Reading Propolis's change feed into the replica.
pub struct Replica {
    propolis: Arc<crate::propolis::Propolis>,
    /// Woken when an event of Propolis's arrives: there is something to read now.
    pub wake: Notify,
}

impl Replica {
    pub fn new(propolis: Arc<crate::propolis::Propolis>) -> Arc<Self> {
        Arc::new(Self {
            propolis,
            wake: Notify::new(),
        })
    }

    /// Read the feed from where the replica stopped to its end. Returns the changes applied.
    pub async fn sync(&self, db: &DatabaseConnection) -> Result<usize, String> {
        let token = self.propolis.token(SCOPE).await?;
        let mut applied = 0;
        loop {
            let after = cursor(db).await.map_err(|e| e.to_string())?;
            let page: Page = self
                .propolis
                .http()
                .get(format!(
                    "{}/api/v1/changes?after={after}&limit=500",
                    self.propolis.issuer()
                ))
                .bearer_auth(&token)
                .send()
                .await
                .map_err(|e| e.without_url().to_string())?
                .error_for_status()
                .map_err(|e| e.without_url().to_string())?
                .json()
                .await
                .map_err(|e| e.without_url().to_string())?;
            if page.changes.is_empty() {
                return Ok(applied);
            }
            // A page and the cursor after it, together.
            let txn = db.begin().await.map_err(|e| e.to_string())?;
            for entry in &page.changes {
                if let Some(change) = membership(&entry.event) {
                    apply(&txn, &change).await.map_err(|e| e.to_string())?;
                    applied += 1;
                }
            }
            txn.execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "INSERT INTO replica_cursors (feed, seq) VALUES ($1, $2) ON CONFLICT (feed) DO UPDATE SET seq = $2",
                [FEED.into(), page.next.into()],
            ))
            .await
            .map_err(|e| e.to_string())?;
            txn.commit().await.map_err(|e| e.to_string())?;
        }
    }

    /// Read the feed every minute, and at once when an event says there is something new.
    pub fn spawn(self: &Arc<Self>, db: DatabaseConnection) {
        let replica = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                if let Err(error) = replica.sync(&db).await {
                    tracing::warn!(%error, "the members' replica could not read Propolis's changes");
                }
                tokio::select! {
                    _ = replica.wake.notified() => {}
                    _ = tokio::time::sleep(Duration::from_secs(60)) => {}
                }
            }
        });
    }
}

async fn cursor(db: &DatabaseConnection) -> Result<i64, DbErr> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT seq FROM replica_cursors WHERE feed = $1",
            [FEED.into()],
        ))
        .await?;
    row.map(|r| r.try_get("", "seq"))
        .transpose()
        .map(|s| s.unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(kind: &str, payload: Value) -> Value {
        json!({ "kind": kind, "organisation": "6f1c2a0e-6c0a-4f7e-b0f4-5a8d2b1c9e30", "source": "propolis", "payload": payload })
    }

    #[test]
    fn propolis_events_say_who_joins_and_who_leaves() {
        let org: Uuid = "6f1c2a0e-6c0a-4f7e-b0f4-5a8d2b1c9e30".parse().unwrap();
        let pat = Uuid::new_v4();
        let target = json!({ "actor": null, "target": { "kind": "member", "id": pat } });
        assert_eq!(
            membership(&event(
                "propolis.organisation.created",
                json!({ "actor": pat })
            )),
            Some(Membership::Joined {
                organisation: org,
                account: pat
            })
        );
        for kind in ["propolis.member.joined", "propolis.member.reactivated"] {
            assert_eq!(
                membership(&event(kind, target.clone())),
                Some(Membership::Joined {
                    organisation: org,
                    account: pat
                })
            );
        }
        for kind in [
            "propolis.member.left",
            "propolis.member.removed",
            "propolis.member.expired",
            "propolis.member.deleted",
        ] {
            assert_eq!(
                membership(&event(kind, target.clone())),
                Some(Membership::Left {
                    organisation: org,
                    account: pat
                })
            );
        }
        assert_eq!(
            membership(&event("propolis.organisation.deleted", json!({}))),
            Some(Membership::Dissolved { organisation: org })
        );
        for kind in [
            "propolis.member.standing_changed",
            "propolis.role.assigned",
            "drive.member.joined",
        ] {
            assert_eq!(membership(&event(kind, target.clone())), None, "{kind}");
        }
    }
}

//! Server events on their way between services (design `docs/design/hub.md`, section 7; ADR 0015 of
//! `Ruchoir/.github`).
//!
//! - `event_subscriptions`: which service receives which event kinds, at which endpoint, from the
//!   subscriptions file the installer writes.
//! - `event_deliveries`: one per event and subscriber, holding the exact bytes and signature the
//!   emitter sent, until delivered or given up after a day.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                r#"
CREATE TABLE event_subscriptions (
    subscriber text NOT NULL,
    kind text NOT NULL,
    -- https, or plain http to the machine itself (a development instance, the tests).
    endpoint text NOT NULL
        CHECK (endpoint LIKE 'https://%' OR endpoint LIKE 'http://127.0.0.1:%' OR endpoint LIKE 'http://localhost:%'),
    PRIMARY KEY (subscriber, kind)
);

CREATE TABLE event_deliveries (
    id uuid PRIMARY KEY,
    event_id uuid NOT NULL,
    kind text NOT NULL,
    subscriber text NOT NULL,
    endpoint text NOT NULL,
    body bytea NOT NULL,
    signature text NOT NULL,
    attempts integer NOT NULL DEFAULT 0,
    next_attempt_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL DEFAULT now() + interval '24 hours',
    delivered_at timestamptz,
    last_error text,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (event_id, subscriber)
);
CREATE INDEX event_deliveries_due ON event_deliveries (next_attempt_at) WHERE delivered_at IS NULL;
"#,
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE IF EXISTS event_deliveries, event_subscriptions")
            .await?;
        Ok(())
    }
}

//! Push and the mail digest (design `docs/design/hub.md`, section 2).
//!
//! The browsers' subscriptions and the phones, as the monolith kept them, under the account; and on
//! each inbox entry, when the digest decided it, so that it is decided once.

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
CREATE TABLE push_subscriptions (
    id uuid PRIMARY KEY,
    account_id uuid NOT NULL,
    endpoint text NOT NULL UNIQUE CHECK (char_length(endpoint) <= 2048),
    p256dh text NOT NULL,
    auth text NOT NULL,
    user_agent text,
    created_at timestamptz NOT NULL DEFAULT now(),
    last_success_at timestamptz,
    failures integer NOT NULL DEFAULT 0
);
CREATE INDEX push_subscriptions_account ON push_subscriptions (account_id);

CREATE TABLE push_devices (
    id uuid PRIMARY KEY,
    account_id uuid NOT NULL,
    platform text NOT NULL CHECK (platform IN ('ios', 'android')),
    token text NOT NULL UNIQUE,
    created_at timestamptz NOT NULL DEFAULT now(),
    last_seen_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX push_devices_account ON push_devices (account_id);

ALTER TABLE inbox_entries ADD COLUMN mail_handled_at timestamptz;
CREATE INDEX inbox_entries_mail_pending ON inbox_entries (created_at) WHERE mail_handled_at IS NULL;
"#,
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "DROP INDEX IF EXISTS inbox_entries_mail_pending; \
                 ALTER TABLE inbox_entries DROP COLUMN IF EXISTS mail_handled_at; \
                 DROP TABLE IF EXISTS push_devices; \
                 DROP TABLE IF EXISTS push_subscriptions;",
            )
            .await?;
        Ok(())
    }
}

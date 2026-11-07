//! The inbox (design `docs/design/hub.md`, sections 2 and 3).
//!
//! An entry holds what its product published: no foreign key to a product, whose tables the hub
//! never reads. The product's object is an opaque id, the entry leads to a link, and what it shows
//! (names, an excerpt) travels in `variables`, as it stood when it was published.

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
CREATE TABLE inbox_entries (
    id uuid PRIMARY KEY,
    account_id uuid NOT NULL,
    product text NOT NULL,
    kind text NOT NULL CHECK (kind ~ '^[a-z][a-z0-9_]*\.[a-z][a-z0-9_.]*$'),
    category text NOT NULL
        CHECK (category IN ('mention', 'reply', 'direct', 'activity', 'reminder', 'invitation', 'notice')),
    organisation_id uuid,
    actor_id uuid,
    object_id text NOT NULL CHECK (char_length(object_id) BETWEEN 1 AND 200),
    link text NOT NULL CHECK (char_length(link) <= 2000),
    variables jsonb NOT NULL DEFAULT '{}',
    created_at timestamptz NOT NULL DEFAULT now(),
    read_at timestamptz
);
CREATE INDEX inbox_entries_feed ON inbox_entries (account_id, created_at DESC, id DESC);
CREATE INDEX inbox_entries_unread ON inbox_entries (account_id) WHERE read_at IS NULL;
CREATE INDEX inbox_entries_object ON inbox_entries (product, object_id);
"#,
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE IF EXISTS inbox_entries")
            .await?;
        Ok(())
    }
}

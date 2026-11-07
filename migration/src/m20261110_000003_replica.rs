//! Who belongs to which organisation, and the presence a person chose (design `docs/design/hub.md`,
//! sections 2 and 5).
//!
//! - `organisation_members`: the hub's replica of the organisations' members, kept from Propolis's
//!   change feed; presence reaches whoever shares an organisation.
//! - `replica_cursors`: how far each feed was read.
//! - `presence_choices`: the manual presence (`active`, `away`, `dnd`, `invisible`), moved from
//!   Propolis's accounts to the hub.

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
CREATE TABLE organisation_members (
    organisation_id uuid NOT NULL,
    account_id uuid NOT NULL,
    PRIMARY KEY (organisation_id, account_id)
);
CREATE INDEX organisation_members_account ON organisation_members (account_id);

CREATE TABLE replica_cursors (
    feed text PRIMARY KEY,
    seq bigint NOT NULL
);

CREATE TABLE presence_choices (
    account_id uuid PRIMARY KEY,
    choice text NOT NULL CHECK (choice IN ('active', 'away', 'dnd', 'invisible')),
    updated_at timestamptz NOT NULL DEFAULT now()
);
"#,
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "DROP TABLE IF EXISTS presence_choices, replica_cursors, organisation_members",
            )
            .await?;
        Ok(())
    }
}

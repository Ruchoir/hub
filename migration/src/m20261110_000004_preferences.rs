//! A person's notification settings and their level per organisation (design `docs/design/hub.md`,
//! section 4).

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
CREATE TABLE notification_settings (
    account_id uuid PRIMARY KEY,
    prefs jsonb NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE organisation_levels (
    account_id uuid NOT NULL,
    organisation_id uuid NOT NULL,
    level text NOT NULL CHECK (level IN ('all', 'mentions', 'none')),
    PRIMARY KEY (account_id, organisation_id)
);
"#,
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE IF EXISTS organisation_levels, notification_settings")
            .await?;
        Ok(())
    }
}

//! The hub's database migrations, applied in order by [`Migrator`].
//!
//! Migrations are forward-only: one that has shipped is never edited, a change is a new one. Each
//! table comes with the code that uses it.

pub use sea_orm_migration::prelude::*;

mod m20261110_000001_inbox;
mod m20261110_000002_events;
mod m20261110_000003_replica;
mod m20261110_000004_preferences;
mod m20261110_000005_push;

/// Every migration of the hub, oldest first.
pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20261110_000001_inbox::Migration),
            Box::new(m20261110_000002_events::Migration),
            Box::new(m20261110_000003_replica::Migration),
            Box::new(m20261110_000004_preferences::Migration),
            Box::new(m20261110_000005_push::Migration),
        ]
    }
}

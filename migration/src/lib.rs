//! The hub's database migrations, applied in order by [`Migrator`].
//!
//! Migrations are forward-only: one that has shipped is never edited, a change is a new one. None yet:
//! the inbox, the push subscriptions and the deliveries come with their code.

pub use sea_orm_migration::prelude::*;

/// Every migration of the hub, oldest first.
pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        Vec::new()
    }
}

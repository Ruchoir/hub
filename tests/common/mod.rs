//! What the integration tests share: a state with no store behind it, and the real stores.
//!
//! The real stores are a throwaway PostgreSQL at `RUCHOIR_TEST_DATABASE_URL` and a Valkey at
//! `RUCHOIR_TEST_VALKEY_URL`, as the shared `rust` workflow of Ruchoir/.github starts them. Without
//! them the tests that need them are skipped, except where `RUCHOIR_TEST_STORES=required` (the CI),
//! where a missing store is a failure: a suite that skips itself reports green while testing
//! nothing. Never point these at a database someone uses.

#![allow(dead_code)]

use std::sync::Arc;

use fred::prelude::Pool;
use hub::{AppState, Config};
use hub_migration::{Migrator, MigratorTrait};
use sea_orm::DatabaseConnection;

/// Applies the migrations once per test binary, so parallel tests never race two runners.
static SCHEMA: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

/// The settings of a test: the hub at `https://hub.example.org`, everything else by default.
pub fn config() -> Config {
    Config::from_lookup(|name| {
        (name == "HUB_PUBLIC_URL").then(|| "https://hub.example.org".to_owned())
    })
    .expect("test configuration")
}

/// A state whose stores are not reachable, for the routes that never touch them.
pub fn offline_state() -> AppState {
    // Built like every pool of the hub (`cache::builder`), so a command gives up after its timeout
    // instead of waiting for a connection that never comes; never initialised, so nothing dials out.
    let valkey = hub::cache::builder("redis://127.0.0.1:1")
        .expect("a Valkey address")
        .build_pool(1)
        .expect("an unconnected pool");
    AppState {
        config: Arc::new(config()),
        db: DatabaseConnection::default(),
        valkey,
    }
}

/// The real stores, migrated, or `None` when this run has none and does not require them.
pub async fn stores() -> Option<(DatabaseConnection, Pool)> {
    let database_url = non_empty("RUCHOIR_TEST_DATABASE_URL");
    let valkey_url = non_empty("RUCHOIR_TEST_VALKEY_URL");
    let (Some(database_url), Some(valkey_url)) = (database_url, valkey_url) else {
        assert!(
            non_empty("RUCHOIR_TEST_STORES").as_deref() != Some("required"),
            "RUCHOIR_TEST_STORES=required, but RUCHOIR_TEST_DATABASE_URL or RUCHOIR_TEST_VALKEY_URL is unset"
        );
        eprintln!("no test stores (RUCHOIR_TEST_DATABASE_URL, RUCHOIR_TEST_VALKEY_URL): skipping");
        return None;
    };
    let config = Config::from_lookup(|name| match name {
        "HUB_DATABASE_URL" => Some(database_url.clone()),
        "HUB_VALKEY_URL" => Some(valkey_url.clone()),
        _ => None,
    })
    .expect("store configuration");
    let db = hub::db::connect(&config)
        .await
        .expect("the test database answers");
    SCHEMA
        .get_or_init(|| async {
            Migrator::up(&db, None).await.expect("the migrations apply");
        })
        .await;
    let valkey = hub::cache::connect(&config)
        .await
        .expect("the test Valkey answers");
    Some((db, valkey))
}

/// A state on the real stores.
pub async fn online_state() -> Option<AppState> {
    let (db, valkey) = stores().await?;
    Some(AppState {
        config: Arc::new(config()),
        db,
        valkey,
    })
}

fn non_empty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

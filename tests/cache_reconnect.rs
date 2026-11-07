//! What a Valkey restart does to the hub: every connection closed under it. A test binary of its
//! own, since it closes the connections of every other client of the test Valkey, and Cargo runs
//! test binaries one after another.
//!
//! From the monolith's `cache.rs`, where it needed a throwaway Valkey at `VALKEY_TEST_URL`.

mod common;

use std::time::{Duration, Instant};

use fred::interfaces::ClientLike;
use fred::prelude::{Builder, Config as ValkeyConfig};

#[tokio::test]
async fn the_pool_answers_again_after_valkey_drops_its_connections() {
    let Some((_, pool)) = common::stores().await else {
        return;
    };
    let url = std::env::var("RUCHOIR_TEST_VALKEY_URL").expect("set when the stores are");
    let _: String = pool.ping(None).await.expect("first ping");

    let killer = Builder::from_config(ValkeyConfig::from_url(&url).unwrap())
        .build()
        .expect("killer client");
    killer.init().await.expect("killer connects");
    let _: i64 = killer
        .custom(
            fred::cmd!("CLIENT"),
            vec!["KILL", "TYPE", "normal", "SKIPME", "yes"],
        )
        .await
        .expect("client kill");

    let started = Instant::now();
    loop {
        let ping = tokio::time::timeout(Duration::from_secs(6), pool.ping::<String>(None)).await;
        if matches!(ping, Ok(Ok(_))) {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "the pool never answered again after its connections were closed"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let _ = killer.quit().await;
}

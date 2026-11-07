//! Valkey (RESP) connectivity via fred.
//!
//! Backs the browser sessions, the sign-in throttle and the short-lived states of the second factors.
//! Dev speaks plaintext RESP over the trusted Docker network; TLS would use fred's
//! `enable-rustls-ring` feature (ring, not AWS `aws-lc-rs`) when hardening later.
//!
//! What happens when Valkey drops every connection is tested in `tests/cache_reconnect.rs`, a test
//! binary of its own, since it closes the connections of every other client.

use fred::prelude::{Builder, ClientLike, Config as ValkeyConfig, Error, Pool};

use crate::config::Config;

/// Build and initialize the Valkey connection pool from configuration.
pub async fn connect(config: &Config) -> Result<Pool, Error> {
    connect_url(&config.valkey_url, config.valkey_pool_size).await
}

/// [`connect`], from the URL and the pool size alone.
pub async fn connect_url(url: &str, pool_size: usize) -> Result<Pool, Error> {
    let pool = builder(url)?.build_pool(pool_size)?;
    // Establish the connections up front so a misconfigured Valkey fails fast at startup.
    pool.init().await?;
    Ok(pool)
}

/// The builder every Valkey client of the hub starts from.
///
/// fred's defaults are no reconnection policy and no command timeout. With them, a Valkey that
/// restarts (an upgrade, a crash, a host reboot) closes every connection for good, and each command
/// afterwards waits forever: sessions, the throttle and the health probe all hang until the service
/// itself is restarted. So every client reconnects, retrying forever with a capped backoff, and no
/// command waits longer than a few seconds for an answer.
pub fn builder(url: &str) -> Result<Builder, Error> {
    let mut builder = Builder::from_config(ValkeyConfig::from_url(url)?);
    // 0 attempts means no limit: Valkey coming back an hour later is still picked up.
    builder.set_policy(fred::prelude::ReconnectPolicy::new_exponential(
        0, 100, 5_000, 2,
    ));
    builder.with_performance_config(|performance| {
        performance.default_command_timeout = std::time::Duration::from_secs(5);
    });
    builder.with_connection_config(|connection| {
        connection.connection_timeout = std::time::Duration::from_secs(5);
    });
    Ok(builder)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn every_client_reconnects_and_bounds_its_commands() {
        let builder = builder("redis://127.0.0.1:6379").expect("builder");
        let policy = builder.get_policy().expect("a reconnection policy");
        assert_eq!(policy.max_attempts(), 0, "retries forever");
        assert_eq!(
            builder.get_performance_config().default_command_timeout,
            Duration::from_secs(5)
        );
    }
}

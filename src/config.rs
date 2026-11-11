//! What the hub reads from its environment at start-up.
//!
//! Every setting has a development default, so `cargo run` works on a fresh clone. A value that is
//! set but cannot be read stops the start-up with the variable's name: a typo in production must not
//! fall back to a default silently.

use std::fmt;
use std::net::SocketAddr;

use ruchoir_http::client_ip::TrustedProxies;
use ruchoir_http::origin::origin_of;

/// Where to listen. `0.0.0.0:8080` in the container image.
pub const LISTEN: &str = "HUB_LISTEN";
/// The address people and services reach the hub at, behind the reverse proxy
/// (`https://hub.example.org`).
pub const PUBLIC_URL: &str = "HUB_PUBLIC_URL";
/// The reverse proxies whose `X-Forwarded-For` is believed: addresses or CIDR ranges, separated by
/// commas. Empty by default: the connection's peer is the client.
pub const TRUSTED_PROXIES: &str = "HUB_TRUSTED_PROXIES";
/// The PostgreSQL database the hub owns.
pub const DATABASE_URL: &str = "HUB_DATABASE_URL";
/// The most connections to the database at once.
pub const DB_MAX_CONNECTIONS: &str = "HUB_DB_MAX_CONNECTIONS";
/// The Valkey cache (presence, live delivery between the hub's processes).
pub const VALKEY_URL: &str = "HUB_VALKEY_URL";
/// How many connections the Valkey pool keeps.
pub const VALKEY_POOL_SIZE: &str = "HUB_VALKEY_POOL_SIZE";
/// Whether pending migrations are applied at start-up: `true` in development, `false` in production,
/// where `hub migrate` applies them deliberately.
pub const AUTO_MIGRATE: &str = "HUB_AUTO_MIGRATE";
/// Propolis's issuer, whose access tokens the hub accepts. Unset: every authenticated call is
/// refused.
pub const ISSUER: &str = "HUB_ISSUER";
/// The hub's client id at Propolis: the audience of the logout tokens it receives.
pub const CLIENT_ID: &str = "HUB_CLIENT_ID";
/// The origins of the instance's product pages, comma-separated: the only pages that may open a
/// socket.
pub const PAGE_ORIGINS: &str = "HUB_PAGE_ORIGINS";
/// The subscriptions file the installer writes: which service receives which event kinds.
pub const SUBSCRIPTIONS: &str = "HUB_SUBSCRIPTIONS";
/// The instance's manifest the installer writes (`manifest.json`), served at
/// `/.well-known/ruchoir-manifest`. Unset: that route answers `404`.
pub const MANIFEST: &str = "HUB_MANIFEST";
/// The hub's Ed25519 client key (PEM, PKCS#8) the installer generated, to read Propolis's change
/// feed in its own name.
pub const CLIENT_KEY: &str = "HUB_CLIENT_KEY";
/// The id under which that key is registered with Propolis.
pub const CLIENT_KEY_ID: &str = "HUB_CLIENT_KEY_ID";
/// The instance's VAPID key (RFC 8292), a P-256 private key in PEM (PKCS#8) the installer generated
/// and keeps: the key browsers subscribe against. Unset: no web push.
pub const VAPID_KEY: &str = "HUB_VAPID_KEY";
/// The push services the hub may call, by host or parent domain, comma-separated: an endpoint is a
/// URL the browser chose, so anything else is refused (the browsers' own services by default).
pub const PUSH_HOSTS: &str = "HUB_PUSH_HOSTS";
/// The publisher's relay that wakes the phone apps (ADR 0002). Empty: no phone is woken.
pub const PUSH_RELAY_URL: &str = "HUB_PUSH_RELAY_URL";
/// How long an entry waits unread before it goes into a mail digest, in seconds (15 minutes by
/// default); `0` turns the digest off.
pub const DIGEST_DELAY_SECS: &str = "HUB_DIGEST_DELAY_SECS";

const DEFAULT_LISTEN: &str = "127.0.0.1:8080";
const DEFAULT_PUBLIC_URL: &str = "http://localhost:8080";
const DEFAULT_DATABASE_URL: &str = "postgres://hub:hub@localhost:5432/hub";
const DEFAULT_VALKEY_URL: &str = "redis://localhost:6379";
/// The browsers' own push services: Google's (Chrome, Edge on Android), Mozilla's, Apple's and
/// Microsoft's. The instance does not choose them: each browser brings its vendor's (ADR 0001).
pub const DEFAULT_PUSH_HOSTS: &str =
    "fcm.googleapis.com,push.services.mozilla.com,push.apple.com,notify.windows.com";
/// The publisher's relay (ADR 0002).
pub const DEFAULT_PUSH_RELAY_URL: &str = "https://ruchoir.com/push";

/// The settings of a running the hub. Its `Debug` output leaves out the stores' addresses, which
/// hold passwords.
#[derive(Clone)]
pub struct Config {
    /// The socket address to listen on.
    pub listen: SocketAddr,
    /// The public origin, without a path: `https://hub.example.org`.
    pub public_origin: String,
    /// The reverse proxies trusted to say who the client is.
    pub trusted_proxies: TrustedProxies,
    /// The database's address. Holds a password: never logged.
    pub database_url: String,
    /// The most connections to the database at once (10 by default).
    pub db_max_connections: u32,
    /// The cache's address. May hold a password: never logged.
    pub valkey_url: String,
    /// How many connections the Valkey pool keeps (6 by default).
    pub valkey_pool_size: usize,
    /// Whether pending migrations are applied at start-up.
    pub auto_migrate: bool,
    /// Propolis's issuer, when the hub accepts its tokens.
    pub issuer: Option<String>,
    /// The hub's client id at Propolis.
    pub client_id: Option<String>,
    /// The product pages' origins.
    pub page_origins: Vec<String>,
    /// The subscriptions file, when there is one.
    pub subscriptions: Option<std::path::PathBuf>,
    /// The manifest file, when the hub serves one.
    pub manifest: Option<std::path::PathBuf>,
    /// The hub's client key file and its key id, when it reads Propolis's change feed.
    pub client_key: Option<(std::path::PathBuf, String)>,
    /// The VAPID key file, when the hub sends web push.
    pub vapid_key: Option<std::path::PathBuf>,
    /// The push services it may call.
    pub push_hosts: Vec<String>,
    /// The phones' relay, without a trailing slash.
    pub push_relay_url: Option<String>,
    /// How long an entry waits before the digest, in seconds; `0` for no digest.
    pub digest_delay_secs: i64,
}

impl Config {
    /// Reads the settings from the process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Reads the settings through `lookup`, which answers a variable's value when it is set. The
    /// tests pass their own.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let listen = lookup(LISTEN).unwrap_or_else(|| DEFAULT_LISTEN.to_owned());
        let listen = listen
            .parse()
            .map_err(|_| ConfigError::new(LISTEN, "an address and a port, like 0.0.0.0:8080"))?;

        let public_url = lookup(PUBLIC_URL).unwrap_or_else(|| DEFAULT_PUBLIC_URL.to_owned());
        let public_origin = origin_of(&public_url)
            .filter(|origin| origin.starts_with("https://") || origin.starts_with("http://"))
            .ok_or_else(|| ConfigError::new(PUBLIC_URL, "an absolute http or https URL"))?;

        let trusted_proxies = match lookup(TRUSTED_PROXIES) {
            Some(value) if !value.trim().is_empty() => value.parse().map_err(|_| {
                ConfigError::new(
                    TRUSTED_PROXIES,
                    "addresses or CIDR ranges, separated by commas",
                )
            })?,
            _ => TrustedProxies::none(),
        };

        let database_url = lookup(DATABASE_URL).unwrap_or_else(|| DEFAULT_DATABASE_URL.to_owned());
        if !database_url.starts_with("postgres://") && !database_url.starts_with("postgresql://") {
            return Err(ConfigError::new(DATABASE_URL, "a postgres:// URL"));
        }
        let valkey_url = lookup(VALKEY_URL).unwrap_or_else(|| DEFAULT_VALKEY_URL.to_owned());
        if !valkey_url.starts_with("redis://") && !valkey_url.starts_with("rediss://") {
            return Err(ConfigError::new(VALKEY_URL, "a redis:// or rediss:// URL"));
        }

        Ok(Self {
            listen,
            public_origin,
            trusted_proxies,
            database_url,
            db_max_connections: positive(&lookup, DB_MAX_CONNECTIONS, 10)?,
            valkey_url,
            valkey_pool_size: positive(&lookup, VALKEY_POOL_SIZE, 6)?,
            auto_migrate: match lookup(AUTO_MIGRATE).as_deref() {
                None | Some("true") => true,
                Some("false") => false,
                Some(_) => return Err(ConfigError::new(AUTO_MIGRATE, "true or false")),
            },
            issuer: lookup(ISSUER)
                .map(|v| v.trim().trim_end_matches('/').to_owned())
                .filter(|v| !v.is_empty()),
            client_id: lookup(CLIENT_ID)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty()),
            page_origins: lookup(PAGE_ORIGINS)
                .unwrap_or_default()
                .split(',')
                .map(|o| o.trim().trim_end_matches('/').to_owned())
                .filter(|o| !o.is_empty())
                .collect(),
            subscriptions: lookup(SUBSCRIPTIONS)
                .filter(|v| !v.trim().is_empty())
                .map(std::path::PathBuf::from),
            manifest: lookup(MANIFEST)
                .filter(|v| !v.trim().is_empty())
                .map(std::path::PathBuf::from),
            client_key: match (lookup(CLIENT_KEY), lookup(CLIENT_KEY_ID)) {
                (Some(path), Some(kid)) if !path.trim().is_empty() && !kid.trim().is_empty() => {
                    Some((std::path::PathBuf::from(path), kid.trim().to_owned()))
                }
                _ => None,
            },
            vapid_key: lookup(VAPID_KEY)
                .filter(|v| !v.trim().is_empty())
                .map(std::path::PathBuf::from),
            push_hosts: lookup(PUSH_HOSTS)
                .unwrap_or_else(|| DEFAULT_PUSH_HOSTS.to_owned())
                .split(',')
                .map(|h| h.trim().trim_start_matches('.').to_ascii_lowercase())
                .filter(|h| !h.is_empty())
                .collect(),
            push_relay_url: match lookup(PUSH_RELAY_URL)
                .unwrap_or_else(|| DEFAULT_PUSH_RELAY_URL.to_owned())
                .trim()
            {
                "" => None,
                url if url.starts_with("https://") || url.starts_with("http://") => {
                    Some(url.trim_end_matches('/').to_owned())
                }
                _ => {
                    return Err(ConfigError::new(
                        PUSH_RELAY_URL,
                        "an http or https URL, or empty",
                    ))
                }
            },
            digest_delay_secs: match lookup(DIGEST_DELAY_SECS) {
                None => 900,
                Some(text) => text
                    .parse::<i64>()
                    .ok()
                    .filter(|secs| *secs >= 0)
                    .ok_or_else(|| {
                        ConfigError::new(DIGEST_DELAY_SECS, "a whole number of seconds")
                    })?,
            },
        })
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("listen", &self.listen)
            .field("public_origin", &self.public_origin)
            .field("trusted_proxies", &self.trusted_proxies)
            .field("db_max_connections", &self.db_max_connections)
            .field("valkey_pool_size", &self.valkey_pool_size)
            .field("auto_migrate", &self.auto_migrate)
            .finish_non_exhaustive()
    }
}

/// A whole number above zero, or `default` when the variable is unset.
fn positive<T: std::str::FromStr + PartialOrd + From<u8>>(
    lookup: &impl Fn(&str) -> Option<String>,
    variable: &'static str,
    default: T,
) -> Result<T, ConfigError> {
    match lookup(variable) {
        None => Ok(default),
        Some(text) => text
            .parse::<T>()
            .ok()
            .filter(|value| *value > T::from(0))
            .ok_or_else(|| ConfigError::new(variable, "a whole number above zero")),
    }
}

/// A setting that is set but cannot be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    /// The environment variable.
    pub variable: &'static str,
    /// What it should hold.
    pub expected: &'static str,
}

impl ConfigError {
    fn new(variable: &'static str, expected: &'static str) -> Self {
        Self { variable, expected }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} must be {}", self.variable, self.expected)
    }
}

impl std::error::Error for ConfigError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(pairs: &[(&str, &str)]) -> Result<Config, ConfigError> {
        Config::from_lookup(|name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        })
    }

    #[test]
    fn a_fresh_clone_runs_on_the_development_defaults() {
        let config = read(&[]).unwrap();
        assert_eq!(config.listen, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(config.public_origin, "http://localhost:8080");
        assert!(config.trusted_proxies.is_empty());
        assert_eq!(config.database_url, DEFAULT_DATABASE_URL);
        assert_eq!(config.valkey_url, DEFAULT_VALKEY_URL);
        assert_eq!(
            (config.db_max_connections, config.valkey_pool_size),
            (10, 6)
        );
        assert!(config.auto_migrate);
        assert!(config.vapid_key.is_none());
        assert!(config.manifest.is_none());
        assert_eq!(config.push_hosts.len(), 4);
        assert_eq!(
            config.push_relay_url.as_deref(),
            Some(DEFAULT_PUSH_RELAY_URL)
        );
        assert_eq!(config.digest_delay_secs, 900);
    }

    #[test]
    fn push_and_the_digest_are_read_and_checked() {
        let config = read(&[
            (PUSH_HOSTS, " .push.example , other.example"),
            (PUSH_RELAY_URL, "https://relay.example/push/"),
            (DIGEST_DELAY_SECS, "0"),
        ])
        .unwrap();
        assert_eq!(config.push_hosts, ["push.example", "other.example"]);
        assert_eq!(
            config.push_relay_url.as_deref(),
            Some("https://relay.example/push")
        );
        assert_eq!(config.digest_delay_secs, 0);
        assert!(read(&[(PUSH_RELAY_URL, "")])
            .unwrap()
            .push_relay_url
            .is_none());
        for (variable, value) in [(PUSH_RELAY_URL, "relay.example"), (DIGEST_DELAY_SECS, "-1")] {
            assert_eq!(read(&[(variable, value)]).unwrap_err().variable, variable);
        }
    }

    #[test]
    fn the_stores_and_migrations_are_read_and_checked() {
        let config = read(&[
            (DATABASE_URL, "postgres://h:secret@db:5432/hub"),
            (VALKEY_URL, "redis://cache:6379"),
            (DB_MAX_CONNECTIONS, "20"),
            (AUTO_MIGRATE, "false"),
        ])
        .unwrap();
        assert_eq!(config.db_max_connections, 20);
        assert!(!config.auto_migrate);
        for (variable, value) in [
            (DATABASE_URL, "mysql://db/hub"),
            (VALKEY_URL, "http://cache"),
            (DB_MAX_CONNECTIONS, "0"),
            (VALKEY_POOL_SIZE, "many"),
            (AUTO_MIGRATE, "yes"),
        ] {
            assert_eq!(read(&[(variable, value)]).unwrap_err().variable, variable);
        }
    }

    #[test]
    fn the_public_url_is_kept_as_its_origin() {
        let config = read(&[(PUBLIC_URL, "https://hub.example.org/")]).unwrap();
        assert_eq!(config.public_origin, "https://hub.example.org");
    }

    #[test]
    fn a_value_that_cannot_be_read_names_its_variable() {
        assert_eq!(read(&[(LISTEN, "8080")]).unwrap_err().variable, LISTEN);
        assert_eq!(
            read(&[(PUBLIC_URL, "hub.example.org")])
                .unwrap_err()
                .variable,
            PUBLIC_URL
        );
        assert_eq!(
            read(&[(TRUSTED_PROXIES, "not-an-address")])
                .unwrap_err()
                .variable,
            TRUSTED_PROXIES
        );
    }

    #[test]
    fn the_debug_output_holds_no_store_address() {
        let config = read(&[(DATABASE_URL, "postgres://h:secret@db/hub")]).unwrap();
        let shown = format!("{config:?}");
        assert!(!shown.contains("secret"));
    }

    #[test]
    fn trusted_proxies_are_read_from_a_list() {
        let config = read(&[(TRUSTED_PROXIES, "10.0.0.0/8, 192.0.2.7")]).unwrap();
        assert!(config.trusted_proxies.trusts("10.1.2.3".parse().unwrap()));
        assert!(config.trusted_proxies.trusts("192.0.2.7".parse().unwrap()));
        assert!(!config
            .trusted_proxies
            .trusts("198.51.100.1".parse().unwrap()));
    }
}

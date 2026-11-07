//! The hub: notifications and real time for every Ruchoir instance (ADR 0003 of Ruchoir/.github).
//!
//! [`run`] reads the configuration, sets up the logs, connects to PostgreSQL and Valkey, applies the
//! migrations when asked to, and serves [`app`] until Ctrl-C or SIGTERM, letting requests in flight
//! finish. `hub migrate` applies the pending migrations and exits: the deliberate path of a
//! production upgrade. [`app`] is the whole HTTP surface, which the tests drive without a socket.
//!
//! The skeleton is Propolis's (Ruchoir/propolis, step 2.1 of the restructuring).

pub mod auth;
pub mod cache;
pub mod config;
pub mod db;
pub mod health;
pub mod inbox;
pub mod openapi;
pub mod product;

use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;

use axum::extract::FromRef;
use axum::http::HeaderValue;
use axum::routing::get;
use axum::Router;
use fred::prelude::Pool;
use hub_migration::{Migrator, MigratorTrait};
use ruchoir_http::client_ip::TrustedProxies;
use ruchoir_http::trace::TraceContextLayer;
use ruchoir_http::{security_headers, Problem, SameSiteGuard};
use sea_orm::DatabaseConnection;
use tracing_subscriber::{fmt, EnvFilter};

pub use config::{Config, ConfigError};

/// The policy of every response: the API serves no page, so nothing may load or frame it.
const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; frame-ancestors 'none'";

/// What every handler can reach.
#[derive(Clone)]
pub struct AppState {
    /// The settings read at start-up.
    pub config: Arc<Config>,
    /// The PostgreSQL pool.
    pub db: DatabaseConnection,
    /// The Valkey pool.
    pub valkey: Pool,
    /// Checks Propolis's access tokens for the hub; `None` without `HUB_ISSUER`.
    pub verifier: Option<ruchoir_auth::Verifier>,
}

impl FromRef<AppState> for TrustedProxies {
    fn from_ref(state: &AppState) -> Self {
        state.config.trusted_proxies.clone()
    }
}

/// The HTTP surface of the hub, wrapped in the shared layers of `ruchoir-http`.
///
/// From the outside in: the trace context, the security headers, then the guard that refuses a
/// write a browser sent from another site. A path no route serves answers `404` as Problem Details.
pub fn app(state: AppState) -> Router {
    let guard = SameSiteGuard::new(&state.config.public_origin);
    Router::new()
        .route("/healthz", get(health::healthz))
        .route("/api/v1/health", get(health::health))
        .route("/api/v1/openapi.json", get(openapi::contract))
        .route("/.well-known/ruchoir-product", get(product::describe))
        .merge(inbox::router())
        .fallback(|| async { Problem::not_found() })
        .with_state(state)
        .layer(guard)
        .layer(security_headers(HeaderValue::from_static(
            CONTENT_SECURITY_POLICY,
        )))
        .layer(TraceContextLayer::new())
}
/// Runs the hub (or `hub migrate`) until it is asked to stop, and says how it ended.
pub async fn run() -> ExitCode {
    init_tracing();
    let config = match Config::from_env() {
        Ok(config) => config,
        Err(error) => {
            tracing::error!(%error, "cannot start");
            return ExitCode::FAILURE;
        }
    };
    let outcome = match std::env::args().nth(1).as_deref() {
        None => start(config).await,
        Some("migrate") => migrate(&config).await,
        Some(other) => Err(format!("unknown command {other:?}; the only one is `migrate`").into()),
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "stopped");
            ExitCode::FAILURE
        }
    }
}

/// Why the start-up or a command stopped.
pub type Failure = Box<dyn std::error::Error>;

/// `hub migrate`: applies the pending migrations, then exits.
async fn migrate(config: &Config) -> Result<(), Failure> {
    let db = db::connect(config).await?;
    Migrator::up(&db, None).await?;
    tracing::info!("migrations applied");
    Ok(())
}

/// Connects to the stores, applies the migrations in development, and serves.
async fn start(config: Config) -> Result<(), Failure> {
    // Both stores are reached before listening, so a wrong address fails the start-up instead of
    // every request.
    let db = db::connect(&config).await?;
    let valkey = cache::connect(&config).await?;
    if config.auto_migrate {
        Migrator::up(&db, None).await?;
        tracing::info!("migrations applied at start-up (HUB_AUTO_MIGRATE)");
    }
    let verifier = match &config.issuer {
        // The hub's resource identifier is its public origin: the audience of its tokens.
        Some(issuer) => Some(ruchoir_auth::Verifier::new(
            issuer.clone(),
            config.public_origin.clone(),
            ruchoir_auth::KeySet::of_issuer(issuer, ruchoir_auth::http_client()?),
        )),
        None => {
            tracing::warn!("HUB_ISSUER unset: every authenticated call is refused");
            None
        }
    };
    let state = AppState {
        config: Arc::new(config),
        db,
        valkey,
        verifier,
    };
    serve(state).await?;
    Ok(())
}

/// Structured logs, filtered by `RUST_LOG` (`info` by default). Never a secret in them. Colours
/// only on a terminal: a container's log collector stores what it is given.
fn init_tracing() {
    let filter = EnvFilter::try_from_env("RUST_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let colours = std::io::IsTerminal::is_terminal(&std::io::stdout());
    fmt().with_env_filter(filter).with_ansi(colours).init();
}

async fn serve(state: AppState) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(state.config.listen).await?;
    tracing::info!(
        address = %listener.local_addr()?,
        public = %state.config.public_origin,
        version = env!("CARGO_PKG_VERSION"),
        "listening"
    );
    // The peer address is what `ClientIp` falls back to when no trusted proxy speaks for the client.
    axum::serve(
        listener,
        app(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
}

/// Resolves on Ctrl-C or, on Unix, SIGTERM (what the container runtime sends).
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(%error, "cannot listen for Ctrl-C");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                tracing::error!(%error, "cannot listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("stopping: finishing the requests in flight");
}

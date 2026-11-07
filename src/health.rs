//! The probes the reverse proxy, the container runtime and the installer read.
//!
//! As in the monolith (`http.rs`): `/healthz` says the process is up and never touches anything else,
//! so a database that is down never gets the service restarted in a loop; `/api/v1/health` says
//! whether PostgreSQL and Valkey answer, and is `degraded` when one of them does not.

use axum::extract::State;
use axum::Json;
use fred::interfaces::ClientLike;
use serde::Serialize;
use utoipa::ToSchema;

use crate::AppState;

/// The service name the probes answer with.
pub const SERVICE: &str = "hub";

/// Liveness: always `ok` when it answers.
#[derive(Debug, Serialize, ToSchema)]
pub struct Health {
    /// Always `"ok"` when the endpoint responds.
    #[schema(example = "ok")]
    pub status: &'static str,
    /// The service that answers.
    #[schema(example = "hub")]
    pub service: &'static str,
    /// The version of the running binary.
    #[schema(example = "0.1.0")]
    pub version: &'static str,
}

/// Readiness, with what each store answered.
#[derive(Debug, Serialize, ToSchema)]
pub struct Readiness {
    /// `"ok"` when every store answers, `"degraded"` otherwise.
    #[schema(example = "ok")]
    pub status: &'static str,
    /// The service that answers.
    #[schema(example = "hub")]
    pub service: &'static str,
    /// The version of the running binary.
    #[schema(example = "0.1.0")]
    pub version: &'static str,
    /// PostgreSQL: `"ok"` or `"down"`.
    #[schema(example = "ok")]
    pub database: &'static str,
    /// Valkey: `"ok"` or `"down"`.
    #[schema(example = "ok")]
    pub cache: &'static str,
}

/// Liveness probe, for the container runtime and the reverse proxy.
#[utoipa::path(
    get,
    path = "/healthz",
    tag = "health",
    responses((status = 200, description = "The process is up", body = Health))
)]
pub async fn healthz() -> Json<Health> {
    Json(Health {
        status: "ok",
        service: SERVICE,
        version: env!("CARGO_PKG_VERSION"),
    })
}

/// Readiness probe: whether PostgreSQL and Valkey answer.
#[utoipa::path(
    get,
    path = "/api/v1/health",
    tag = "health",
    responses((status = 200, description = "What each store answered", body = Readiness))
)]
pub async fn health(State(state): State<AppState>) -> Json<Readiness> {
    let (database, cache) = probe(&state).await;
    Json(Readiness {
        status: if database && cache { "ok" } else { "degraded" },
        service: SERVICE,
        version: env!("CARGO_PKG_VERSION"),
        database: up_or_down(database),
        cache: up_or_down(cache),
    })
}

/// Whether PostgreSQL and Valkey answer, in that order.
pub async fn probe(state: &AppState) -> (bool, bool) {
    let database = state.db.ping().await.is_ok();
    // A bare PING: only whether the round trip succeeds matters.
    let cache = state.valkey.ping::<String>(None).await.is_ok();
    (database, cache)
}

fn up_or_down(up: bool) -> &'static str {
    if up {
        "ok"
    } else {
        "down"
    }
}

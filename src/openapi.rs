//! The contract of `/api/v1`, derived from the code with utoipa (ADR 0005 of Ruchoir/.github).
//!
//! It is served at `/api/v1/openapi.json` and recorded in `contract/openapi-v1.json`. A test holds
//! the code to the recorded file: the contract only grows, and what grew is recorded in the same
//! change (`tests/contract.rs`).

use axum::Json;
use utoipa::OpenApi;

/// Every route of the service registers here.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "Hub API",
        description = "Notifications and real time for every Ruchoir instance.",
        license(name = "AGPL-3.0-only", identifier = "AGPL-3.0-only")
    ),
    paths(
        crate::health::healthz,
        crate::health::health,
        crate::product::describe,
        crate::inbox::publish,
        crate::inbox::read,
        crate::inbox::mark_read,
        crate::inbox::mark_all_read,
        crate::live::routes::publish,
        crate::live::routes::backchannel_logout,
        crate::live::tickets::issue,
        crate::live::ws::open,
        crate::live::sse::open,
        crate::events::intake,
    ),
    tags(
        (name = "health", description = "Probes for the container runtime, the proxy and the installer"),
        (name = "product", description = "What the service is and what it can do"),
        (name = "inbox", description = "What a person was told about, published by the products"),
        (name = "live", description = "Frames pushed live to the pages and apps a person has open"),
        (name = "events", description = "Signed server events, delivered to the services that subscribed"),
    )
)]
pub struct ApiDoc;

/// The contract as this build serves it.
pub async fn contract() -> Json<utoipa::openapi::OpenApi> {
    Json(ApiDoc::openapi())
}

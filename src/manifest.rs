//! The instance's manifest at `/.well-known/ruchoir-manifest` (ADR 0004 of Ruchoir/.github): where
//! apps and pages start their discovery.
//!
//! The installer writes `manifest.json` (the installed services and their addresses) and mounts it
//! read-only into the hub's container; `HUB_MANIFEST` names it. The file is small and read on each
//! request, so a product the installer adds later is served without restarting the hub. It is
//! public: anyone may read it, from any page (`Access-Control-Allow-Origin: *`), and caches may keep
//! it for five minutes.

use std::collections::BTreeMap;

use axum::extract::State;
use axum::http::{header, HeaderValue};
use axum::response::{IntoResponse, Response};
use ruchoir_http::Problem;
use serde::Serialize;
use utoipa::ToSchema;

use crate::AppState;

/// How long a cache may keep the manifest.
pub const CACHE_CONTROL: &str = "public, max-age=300";

/// The manifest of an instance, as the installer writes it. It describes the contract only: the
/// file is served as written.
#[derive(Debug, Serialize, ToSchema)]
pub struct Manifest {
    /// The format's version.
    #[schema(example = 1)]
    pub version: u32,
    /// The instance's domain.
    #[schema(example = "example.org")]
    pub domain: String,
    /// The installed services by name, each with its public address.
    pub services: BTreeMap<String, ManifestService>,
}

/// One installed service.
#[derive(Debug, Serialize, ToSchema)]
pub struct ManifestService {
    /// Where the service answers; it describes itself at `/.well-known/ruchoir-product` there.
    #[schema(example = "https://hub.example.org")]
    pub url: String,
}

/// The instance's manifest: the installed services and their addresses.
#[utoipa::path(
    get,
    path = "/.well-known/ruchoir-manifest",
    tag = "product",
    responses(
        (status = 200, description = "The manifest the installer wrote", body = Manifest),
        (status = 404, description = "This hub serves no manifest (`HUB_MANIFEST` unset)"),
        (status = 500, description = "The manifest cannot be read or is not JSON"),
    )
)]
pub async fn serve(State(state): State<AppState>) -> Response {
    let Some(path) = &state.config.manifest else {
        return Problem::not_found().into_response();
    };
    // A small file: read off the async workers all the same.
    let read = {
        let path = path.clone();
        tokio::task::spawn_blocking(move || std::fs::read(path)).await
    };
    let bytes = match read {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(error)) => {
            tracing::error!(%error, path = %path.display(), "cannot read the manifest");
            return Problem::internal().into_response();
        }
        Err(error) => {
            tracing::error!(%error, "the manifest's read stopped");
            return Problem::internal().into_response();
        }
    };
    if let Err(error) = serde_json::from_slice::<serde_json::Value>(&bytes) {
        tracing::error!(%error, path = %path.display(), "the manifest is not JSON");
        return Problem::internal().into_response();
    }
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            ),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static(CACHE_CONTROL),
            ),
            (
                header::ACCESS_CONTROL_ALLOW_ORIGIN,
                HeaderValue::from_static("*"),
            ),
        ],
        bytes,
    )
        .into_response()
}

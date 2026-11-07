//! What this service says about itself at `/.well-known/ruchoir-product` (ADR 0004 of
//! Ruchoir/.github): its name, its version, the API majors it serves and its features.
//!
//! A client or another service decides from `features`, never from `version`: an instance may run
//! products of different versions. A feature name is announced once it works, and never withdrawn
//! within a major version of the API.

use axum::Json;
use serde::Serialize;
use utoipa::ToSchema;

use crate::health::SERVICE;

/// The features this build serves, in `snake_case`. Empty until the first one lands.
pub const FEATURES: &[&str] = &[];

/// The API majors this build serves, each under `/api/<major>/`.
pub const API_VERSIONS: &[&str] = &["v1"];

/// The description of a Ruchoir product or service.
#[derive(Debug, Serialize, ToSchema)]
pub struct ProductDescription {
    /// The product's name, as the manifest of the instance lists it.
    #[schema(example = "hub")]
    pub name: &'static str,
    /// The version of the running build.
    #[schema(example = "0.1.0")]
    pub version: &'static str,
    /// The API majors served, each under `/api/<major>/`.
    #[schema(example = json!(["v1"]))]
    pub api: &'static [&'static str],
    /// The features served. A client checks one before relying on it.
    #[schema(example = json!([]))]
    pub features: &'static [&'static str],
}

/// What this service is and what it can do.
#[utoipa::path(
    get,
    path = "/.well-known/ruchoir-product",
    tag = "product",
    responses((status = 200, description = "The service's description", body = ProductDescription))
)]
pub async fn describe() -> Json<ProductDescription> {
    Json(ProductDescription {
        name: SERVICE,
        version: env!("CARGO_PKG_VERSION"),
        api: API_VERSIONS,
        features: FEATURES,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn snake_case_names_listed_once(names: &[&str]) -> bool {
        let unique: BTreeSet<&str> = names.iter().copied().collect();
        unique.len() == names.len()
            && names.iter().all(|name| {
                !name.is_empty()
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            })
    }

    #[test]
    fn features_are_snake_case_names_listed_once() {
        assert!(snake_case_names_listed_once(FEATURES));
        assert!(!snake_case_names_listed_once(&["sign_in", "sign_in"]));
        assert!(!snake_case_names_listed_once(&["SignIn"]));
    }

    #[test]
    fn api_versions_are_majors() {
        assert!(API_VERSIONS.iter().all(|v| v
            .strip_prefix('v')
            .is_some_and(|n| n.parse::<u32>().is_ok())));
    }
}

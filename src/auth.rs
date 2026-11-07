//! Who calls the hub (design `docs/design/hub.md`, section 8; ADR 0006 of `Ruchoir/.github`).
//!
//! Every call carries a Propolis access token for the hub (its audience is the hub's public origin),
//! checked by `ruchoir-auth` the way every product checks one:
//!
//! - [`Publisher`]: a product in its own name (`client_credentials`) holding `hub:publish`. What it
//!   publishes (inbox entries, frames, events) is its own; the hub never decides who may see what.
//! - [`Person`]: a person, through their product's backend or an app. They reach only their own
//!   inbox, settings and socket.
//!
//! A refusal answers `401` with `WWW-Authenticate: Bearer error="invalid_token"` (no token, or one
//! that does not hold), or `403` with `error="insufficient_scope"` (a valid token for something
//! else). Without `HUB_ISSUER`, every authenticated call is refused.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{header, HeaderValue};
use axum::response::{IntoResponse, Response};
use ruchoir_auth::access::bearer;
use ruchoir_auth::AccessToken;
use ruchoir_http::Problem;
use uuid::Uuid;

use crate::AppState;

/// The scope a product needs to publish to the hub.
pub const PUBLISH: &str = "hub:publish";

/// Why a call is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// No token, or one that does not hold.
    Unauthenticated,
    /// A valid token, for something this route does not do.
    Forbidden,
}

impl IntoResponse for Refused {
    fn into_response(self) -> Response {
        let (problem, challenge) = match self {
            Refused::Unauthenticated => (Problem::unauthorized(), "Bearer error=\"invalid_token\""),
            Refused::Forbidden => (
                Problem::forbidden().with_reason("insufficient_scope"),
                "Bearer error=\"insufficient_scope\"",
            ),
        };
        let mut response = problem.into_response();
        response.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static(challenge),
        );
        response
    }
}

/// The checked token of a call.
async fn token(parts: &Parts, state: &AppState) -> Result<AccessToken, Refused> {
    let verifier = state.verifier.as_ref().ok_or(Refused::Unauthenticated)?;
    let presented = parts
        .headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(bearer)
        .ok_or(Refused::Unauthenticated)?;
    verifier.verify(presented).await.map_err(|why| {
        tracing::debug!(%why, "token refused");
        Refused::Unauthenticated
    })
}

/// A product publishing in its own name.
#[derive(Debug, Clone, PartialEq)]
pub struct Publisher {
    /// The product's client id at Propolis: the prefix of what it may publish.
    pub client_id: String,
    pub token: AccessToken,
}

impl FromRequestParts<AppState> for Publisher {
    type Rejection = Refused;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Refused> {
        let token = token(parts, state).await?;
        // A person's token never publishes, whatever its scopes: publishing is a service's act.
        if token.is_person() || !token.has_scope(PUBLISH) {
            return Err(Refused::Forbidden);
        }
        Ok(Self {
            client_id: token.client_id.clone(),
            token,
        })
    }
}

/// A person, reaching what is theirs.
#[derive(Debug, Clone, PartialEq)]
pub struct Person {
    /// Their account, as the instance's own services know it (public subjects).
    pub account: Uuid,
    pub token: AccessToken,
}

impl FromRequestParts<AppState> for Person {
    type Rejection = Refused;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Refused> {
        let token = token(parts, state).await?;
        if !token.is_person() {
            return Err(Refused::Forbidden);
        }
        // The hub is one of the instance's own services: its tokens name the account itself.
        let account = Uuid::parse_str(&token.sub).map_err(|_| Refused::Unauthenticated)?;
        Ok(Self { account, token })
    }
}

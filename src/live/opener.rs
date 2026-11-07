//! Who opens a socket or a stream (ADR 0016 of `Ruchoir/.github`).
//!
//! - **A page**: `?ticket=…`, the single-use ticket its product's backend got for the person. A
//!   browser always sends `Origin` on a WebSocket handshake, which has no CORS: it must be one of the
//!   instance's own page origins (`HUB_PAGE_ORIGINS`), or a page elsewhere could ride a ticket.
//! - **An app**: `Authorization: Bearer`, its access token for the hub, for the person it signs in.

use axum::http::{header, HeaderMap};
use axum::response::{IntoResponse, Response};
use ruchoir_auth::access::bearer;
use ruchoir_http::Problem;
use serde::Deserialize;
use utoipa::IntoParams;
use uuid::Uuid;

use super::tickets;
use crate::AppState;

/// How a socket or stream is opened.
#[derive(Debug, Deserialize, IntoParams)]
pub struct OpenQuery {
    /// A page's single-use ticket.
    pub ticket: Option<String>,
}

/// Who opened it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opener {
    pub account: Uuid,
    /// The Propolis session it follows, when known.
    pub sid: Option<String>,
}

/// Why it may not open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Unauthenticated,
    ForeignOrigin,
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        match self {
            Refusal::Unauthenticated => Problem::unauthorized().into_response(),
            Refusal::ForeignOrigin => Problem::forbidden().with_reason("origin").into_response(),
        }
    }
}

/// The opener of a socket or stream, from its handshake.
pub async fn opener(
    state: &AppState,
    headers: &HeaderMap,
    query: &OpenQuery,
) -> Result<Opener, Refusal> {
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    if let Some(origin) = origin {
        if !state.config.page_origins.iter().any(|o| o == origin) {
            tracing::warn!(
                origin,
                "a socket from an origin the instance does not serve"
            );
            return Err(Refusal::ForeignOrigin);
        }
    }
    if let Some(ticket) = &query.ticket {
        let holder = tickets::take(state, ticket)
            .await
            .ok_or(Refusal::Unauthenticated)?;
        return Ok(Opener {
            account: holder.account,
            sid: holder.sid,
        });
    }
    // An app's token; a page never has one, so a browser with an `Origin` must use a ticket.
    if origin.is_some() {
        return Err(Refusal::Unauthenticated);
    }
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(bearer)
        .ok_or(Refusal::Unauthenticated)?;
    let verifier = state.verifier.as_ref().ok_or(Refusal::Unauthenticated)?;
    let checked = verifier
        .verify(token)
        .await
        .map_err(|_| Refusal::Unauthenticated)?;
    if !checked.is_person() {
        return Err(Refusal::Unauthenticated);
    }
    let account = Uuid::parse_str(&checked.sub).map_err(|_| Refusal::Unauthenticated)?;
    Ok(Opener {
        account,
        sid: checked.sid,
    })
}

//! Tickets to open a socket (ADR 0016 of `Ruchoir/.github`).
//!
//! A product page holds no token, so its product's backend asks the hub for a ticket, with a token it
//! exchanged for the signed-in person (RFC 8693), and hands it to the page. A ticket is 256 bits,
//! lives a minute, is kept as its digest only, and is taken by the first socket that presents it. It
//! names the person, the product and the Propolis session, and nothing else.

use axum::extract::State;
use axum::Json;
use fred::interfaces::KeysInterface;
use fred::types::Expiration;
use ruchoir_http::Problem;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::auth::Person;
use crate::AppState;

/// How long a ticket waits for its socket.
pub const TICKET_SECS: i64 = 60;

const PREFIX: &str = "live_ticket:";

/// What a ticket stands for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holder {
    pub account: Uuid,
    /// The product whose page opens the socket.
    pub product: String,
    /// The Propolis session, so the socket closes with it.
    pub sid: Option<String>,
}

fn digest(ticket: &str) -> String {
    Sha256::digest(ticket.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn random() -> Option<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).ok()?;
    Some(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// A new ticket.
#[derive(Debug, Serialize, ToSchema)]
pub struct Ticket {
    pub ticket: String,
    /// Seconds it waits.
    pub expires_in: i64,
}

/// Get a page's product a ticket for the person it acts for.
#[utoipa::path(
    post,
    path = "/api/v1/live/tickets",
    tag = "live",
    responses(
        (status = 200, description = "A single-use ticket, a minute long", body = Ticket),
        (status = 401, description = "No valid token"),
        (status = 403, description = "Not a token a product exchanged for a person")
    )
)]
pub async fn issue(State(state): State<AppState>, person: Person) -> Result<Json<Ticket>, Problem> {
    // A page's ticket comes from its product's backend, acting for the person: an exchanged token.
    // An app holds its own token and opens its socket with it.
    let Some(product) = person.token.actor.clone() else {
        return Err(Problem::forbidden().with_reason("exchanged_token_expected"));
    };
    let holder = Holder {
        account: person.account,
        product,
        sid: person.token.sid.clone(),
    };
    let ticket = random().ok_or_else(Problem::internal)?;
    let json = serde_json::to_string(&holder).map_err(|_| Problem::internal())?;
    let _: () = state
        .valkey
        .set(
            format!("{PREFIX}{}", digest(&ticket)).as_str(),
            json,
            Some(Expiration::EX(TICKET_SECS)),
            None,
            false,
        )
        .await
        .map_err(|error| {
            tracing::error!(%error, "ticket store");
            Problem::internal()
        })?;
    Ok(Json(Ticket {
        ticket,
        expires_in: TICKET_SECS,
    }))
}

/// Take a ticket: once, and only once.
pub async fn take(state: &AppState, ticket: &str) -> Option<Holder> {
    if ticket.len() != 64 || !ticket.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let json: Option<String> = state
        .valkey
        .getdel(format!("{PREFIX}{}", digest(ticket)).as_str())
        .await
        .ok()?;
    json.and_then(|j| serde_json::from_str(&j).ok())
}

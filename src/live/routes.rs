//! The live routes: what products publish, how sockets open, and how they close with their session.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use ruchoir_events::Publication;
use ruchoir_http::Problem;
use uuid::Uuid;

use super::{sse, tickets, ws};
use crate::auth::Publisher;
use crate::AppState;

/// The most accounts one publication may name.
pub const MAX_AUDIENCE: usize = 10_000;

/// Push a frame to the audience a product computed.
#[utoipa::path(
    post,
    path = "/api/v1/live/publish",
    tag = "live",
    request_body(content = Object, description = "A `Publication` of ruchoir-events: `audience` and `frame`"),
    responses(
        (status = 202, description = "Handed to every process"),
        (status = 400, description = "Refused (`reason`: `frame_type_not_yours`, `audience`)"),
        (status = 401, description = "No valid token"),
        (status = 403, description = "Not a product with `hub:publish`")
    )
)]
pub async fn publish(
    State(state): State<AppState>,
    publisher: Publisher,
    Json(publication): Json<Publication>,
) -> Result<StatusCode, Problem> {
    // A product's frames carry its name, so no product speaks for another to their clients.
    let prefix = format!("{}.", publisher.client_id);
    if !publication.frame.frame_type.starts_with(&prefix) {
        return Err(Problem::bad_request().with_reason("frame_type_not_yours"));
    }
    let mut audience = publication.audience;
    audience.sort_unstable();
    audience.dedup();
    if audience.is_empty() || audience.len() > MAX_AUDIENCE {
        return Err(Problem::bad_request().with_reason("audience"));
    }
    state.fanout.publish(audience, publication.frame).await;
    Ok(StatusCode::ACCEPTED)
}

/// Propolis's back-channel logout (OpenID Connect Back-Channel Logout 1.0): a session ended, so the
/// sockets it opened close, on every process; with no `sid`, every socket of the person.
#[utoipa::path(
    post,
    path = "/api/v1/backchannel-logout",
    tag = "live",
    request_body(content = String, content_type = "application/x-www-form-urlencoded", description = "`logout_token`"),
    responses(
        (status = 200, description = "Done"),
        (status = 400, description = "No logout token, or one that does not hold")
    )
)]
pub async fn backchannel_logout(State(state): State<AppState>, body: Bytes) -> StatusCode {
    let Some(token) = form_urlencoded::parse(&body)
        .find(|(k, _)| k == "logout_token")
        .map(|(_, v)| v.into_owned())
    else {
        return StatusCode::BAD_REQUEST;
    };
    let (Some(verifier), Some(client_id)) = (&state.verifier, &state.config.client_id) else {
        return StatusCode::BAD_REQUEST;
    };
    let notice =
        match ruchoir_auth::logout::verify(verifier.keys(), verifier.issuer(), client_id, &token)
            .await
        {
            Ok(notice) => notice,
            Err(why) => {
                tracing::info!(%why, "a logout token refused");
                return StatusCode::BAD_REQUEST;
            }
        };
    match (&notice.sid, Uuid::parse_str(&notice.sub)) {
        (Some(sid), _) => state.fanout.close_session(sid).await,
        (None, Ok(account)) => state.fanout.close_account(account).await,
        (None, Err(_)) => return StatusCode::BAD_REQUEST,
    }
    StatusCode::OK
}

/// The live routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/live/publish", post(publish))
        .route("/api/v1/live/tickets", post(tickets::issue))
        .route("/api/v1/live/ws", get(ws::open))
        .route("/api/v1/live/sse", get(sse::open))
        .route("/api/v1/backchannel-logout", post(backchannel_logout))
}

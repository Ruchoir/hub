//! The server-sent event stream: the same frames as the WebSocket, for a client that cannot hold
//! one (a proxy that drops upgrades). Opened the same way ([`super::opener`]).

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

use super::fanout::ConnId;
use super::opener::{opener, OpenQuery};
use super::presence;
use super::ws::{left, HEARTBEAT_SECS};
use crate::AppState;

/// Forgets the connection when the client goes and the response is dropped.
struct Guard {
    state: AppState,
    account: Uuid,
    id: ConnId,
    heartbeat: JoinHandle<()>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.heartbeat.abort();
        let (state, account, id) = (self.state.clone(), self.account, self.id);
        // `Drop` cannot wait: the cleanup is a task.
        tokio::spawn(async move { left(&state, account, id).await });
    }
}

/// Open the stream.
#[utoipa::path(
    get,
    path = "/api/v1/live/sse",
    tag = "live",
    params(OpenQuery),
    responses(
        (status = 200, description = "A stream of frames"),
        (status = 401, description = "No ticket or token, or not a valid one"),
        (status = 403, description = "A page origin the instance does not serve")
    )
)]
pub async fn open(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<OpenQuery>,
) -> Response {
    let opener = match opener(&state, &headers, &query).await {
        Ok(opener) => opener,
        Err(refusal) => return refusal.into_response(),
    };
    let account = opener.account;
    let (id, rx) = state.fanout.register(account, opener.sid);
    presence::arrived(&state, account).await;
    let beat_state = state.clone();
    let heartbeat = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(HEARTBEAT_SECS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            presence::heartbeat(beat_state.fanout.valkey(), account).await;
        }
    });
    let guard = Guard {
        state,
        account,
        id,
        heartbeat,
    };
    let stream = ReceiverStream::new(rx).map(move |frame| {
        // The guard lives as long as the stream, so its `Drop` runs when the client goes.
        let _alive = &guard;
        let event = Event::default().event(frame.frame_type.clone());
        Ok::<_, Infallible>(match serde_json::to_string(&frame) {
            Ok(json) => event.data(json),
            Err(_) => event.data("{}"),
        })
    });
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(HEARTBEAT_SECS)))
        .into_response()
}

//! The WebSocket: one per page or app, receiving the frames addressed to its person (design, section
//! 6; ADR 0016).
//!
//! Who opens it is settled on the upgrade request, before the socket exists ([`super::opener`]): a
//! page with its single-use ticket, from one of the instance's own page origins, or an app with its
//! token for the hub. The socket only receives, apart from its pings: typing goes from the page to its
//! product, which publishes it. A socket that says nothing for two heartbeats is closed, as before: a
//! half-open socket answers no ping.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use uuid::Uuid;

use super::opener::{opener, OpenQuery, Opener};
use super::presence;
use crate::AppState;

/// How often a socket is pinged, and its presence refreshed.
pub const HEARTBEAT_SECS: u64 = 45;

/// Open the socket.
#[utoipa::path(
    get,
    path = "/api/v1/live/ws",
    tag = "live",
    params(OpenQuery),
    responses(
        (status = 101, description = "Switching to WebSocket"),
        (status = 401, description = "No ticket or token, or not a valid one"),
        (status = 403, description = "A page origin the instance does not serve")
    )
)]
pub async fn open(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<OpenQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    match opener(&state, &headers, &query).await {
        Ok(opener) => ws.on_upgrade(move |socket| serve(state, opener, socket)),
        Err(refusal) => refusal.into_response(),
    }
}

async fn serve(state: AppState, opener: Opener, socket: WebSocket) {
    let account = opener.account;
    let (id, mut rx) = state.fanout.register(account, opener.sid.clone());
    presence::heartbeat(state.fanout.valkey(), account).await;
    let (mut sink, mut stream) = socket.split();

    let opened = Instant::now();
    let last_frame = Arc::new(AtomicU64::new(0));
    let silence_limit = HEARTBEAT_SECS * 2;

    let outbound_state = state.clone();
    let seen = Arc::clone(&last_frame);
    let mut outbound = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(HEARTBEAT_SECS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                frame = rx.recv() => {
                    // `None`: the fan-out dropped this connection (its session ended).
                    let Some(frame) = frame else { break };
                    let Ok(json) = serde_json::to_string(&frame) else { continue };
                    if sink.send(Message::Text(json.into())).await.is_err() {
                        break;
                    }
                }
                _ = ticker.tick() => {
                    let quiet = opened.elapsed().as_secs().saturating_sub(seen.load(Ordering::Relaxed));
                    if quiet >= silence_limit {
                        tracing::debug!(%account, quiet, "closing a socket that stopped answering");
                        break;
                    }
                    if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                        break;
                    }
                    presence::heartbeat(outbound_state.fanout.valkey(), account).await;
                }
            }
        }
        let _ = sink.send(Message::Close(None)).await;
    });

    let inbound_seen = Arc::clone(&last_frame);
    let mut inbound = tokio::spawn(async move {
        while let Some(Ok(message)) = stream.next().await {
            // Any frame proves the other end is there, a pong as much as anything.
            inbound_seen.store(opened.elapsed().as_secs(), Ordering::Relaxed);
            if matches!(message, Message::Close(_)) {
                break;
            }
        }
    });

    tokio::select! {
        _ = &mut outbound => inbound.abort(),
        _ = &mut inbound => outbound.abort(),
    }
    left(&state, account, id).await;
}

/// A connection ended: forget it, and the person's heartbeat with their last one here.
pub async fn left(state: &AppState, account: Uuid, id: super::fanout::ConnId) {
    let was_last = state.fanout.unregister(account, id);
    if was_last && !state.fanout.is_locally_connected(account) {
        presence::clear(state.fanout.valkey(), account).await;
    }
}

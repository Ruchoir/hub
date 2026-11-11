//! Reaching someone who is not looking at Ruchoir (design, section 2).
//!
//! The inbox reaches every open page and app at once (`hub.inbox.new`). This module is what comes
//! after, for the person with no page open:
//!
//! - [`web`], [`vapid`] and [`ece`]: Web Push to the browsers that subscribed (ADR 0001), carrying a
//!   constant encrypted marker and never any content;
//! - [`phone`]: the phone apps, woken through the publisher's relay (ADR 0002), also without
//!   content;
//! - [`words`]: what a tray or a lock screen shows when the device asks, in the person's language.
//!
//! The mail digest of what is still unread is [`crate::digest`].

pub mod ece;
pub mod phone;
pub mod vapid;
pub mod web;
pub mod words;

use axum::routing::{get, post, put};
use axum::Router;

use crate::AppState;

/// What the hub needs to push: its VAPID key, when the instance has one, and an HTTP client that
/// follows no redirect.
pub struct Push {
    pub vapid: Option<vapid::VapidKeys>,
    pub http: reqwest::Client,
}

impl Push {
    pub fn new(vapid: Option<vapid::VapidKeys>, http: reqwest::Client) -> Self {
        Self { vapid, http }
    }

    /// No web push; phones still reach the relay if one is set.
    pub fn without_vapid() -> Self {
        Self {
            vapid: None,
            http: ruchoir_auth::http_client().expect("an HTTP client"),
        }
    }
}

/// The push routes. Merged into the main router with absolute paths.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/push/config", get(web::config))
        .route(
            "/api/v1/me/push/subscription",
            put(web::subscribe).delete(web::unsubscribe),
        )
        .route(
            "/api/v1/me/push/devices",
            put(phone::register).delete(phone::unregister),
        )
        .route("/api/v1/me/push/pending", get(web::pending))
        .route("/api/v1/me/push/test", post(web::test))
        .route("/api/v1/me/push/devices/test", post(phone::test))
}

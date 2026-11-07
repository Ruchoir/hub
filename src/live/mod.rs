//! Live delivery: frames pushed to the pages and apps a person has open (design
//! `docs/design/hub.md`, section 6; ADR 0003 and 0016 of `Ruchoir/.github`).
//!
//! From the monolith's `realtime/`, with the hub's changes: a product publishes a `ClientFrame` for
//! the audience it computed and the hub forwards it unread; a page opens its socket with a ticket its
//! product got, an app with its own token; sockets close with their Propolis session, across
//! processes. Typing stays with the products.
//!
//! - [`fanout`]: this process's connections and the Valkey channel between processes.
//! - [`opener`]: who opens a socket or a stream; [`tickets`]: the pages' single-use tickets.
//! - [`ws`] and [`sse`]: the two transports.
//! - [`presence`]: the live heartbeat.
//! - [`routes`]: publishing, opening, and Propolis's back-channel logout.

pub mod fanout;
pub mod opener;
pub mod presence;
pub mod routes;
pub mod sse;
pub mod tickets;
pub mod ws;

pub use fanout::Fanout;

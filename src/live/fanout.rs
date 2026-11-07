//! The fan-out: which connections this process holds, and the Valkey channel that ties the processes
//! together (design `docs/design/hub.md`, section 6).
//!
//! From the monolith's real-time hub, with two changes:
//!
//! - what travels is a product's [`ClientFrame`] for the audience it computed, which the hub
//!   forwards without reading;
//! - closing is across processes: Propolis's back-channel logout reaches one process, but the
//!   sockets of that session may be open on any. A close goes through the channel like a frame.
//!
//! Each process subscribes to one channel and delivers to its own connections. A connection whose
//! buffer is full loses that frame only; its client resyncs over REST, as before.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use fred::interfaces::{EventInterface, PubsubInterface};
use fred::prelude::{ClientLike, Error, Pool};
use ruchoir_events::ClientFrame;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use uuid::Uuid;

/// The one channel every process listens on.
const CHANNEL: &str = "live:fanout";

/// Frames waiting for one connection before it is considered too slow and loses one.
pub const SEND_BUFFER: usize = 256;

/// A connection's id, unique within this process.
pub type ConnId = u64;

/// What travels on the channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Wire {
    /// A frame for these accounts.
    Frame {
        audience: Vec<Uuid>,
        frame: ClientFrame,
    },
    /// Close what a Propolis session opened, or everything of an account.
    Close {
        #[serde(default)]
        sid: Option<String>,
        #[serde(default)]
        account: Option<Uuid>,
    },
}

/// One open socket or stream.
struct Conn {
    /// The Propolis session it was opened for, when known (a page's ticket, an app's token).
    sid: Option<String>,
    tx: mpsc::Sender<ClientFrame>,
}

/// The connections of this process, by account, and the channel to the others.
pub struct Fanout {
    valkey: Pool,
    connections: Mutex<HashMap<Uuid, HashMap<ConnId, Conn>>>,
    next: AtomicU64,
}

impl Fanout {
    /// Listen on the channel and deliver what comes to this process's connections.
    pub async fn start(valkey_url: &str, valkey: Pool) -> Result<Arc<Self>, Error> {
        let fanout = Self::detached(valkey);
        let subscriber = crate::cache::builder(valkey_url)?.build_subscriber_client()?;
        subscriber.init().await?;
        // Re-subscribes by itself after a reconnection, so a Valkey restart does not leave this
        // process deaf.
        subscriber.manage_subscriptions();
        subscriber.subscribe(CHANNEL).await?;
        let mut rx = subscriber.message_rx();
        let local = Arc::clone(&fanout);
        tokio::spawn(async move {
            // Kept alive as long as the loop: dropping it would close the subscription.
            let _subscriber = subscriber;
            loop {
                match rx.recv().await {
                    Ok(message) => {
                        let Some(text) = message.value.as_bytes_str() else {
                            continue;
                        };
                        match serde_json::from_slice::<Wire>(text.as_bytes()) {
                            Ok(wire) => local.deliver(wire),
                            Err(error) => {
                                tracing::warn!(%error, "dropping a malformed fan-out message")
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "the fan-out subscriber lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        Ok(fanout)
    }

    /// A fan-out that hears nothing from the channel: for tests, and a process that only publishes.
    pub fn detached(valkey: Pool) -> Arc<Self> {
        Arc::new(Self {
            valkey,
            connections: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
        })
    }

    pub fn valkey(&self) -> &Pool {
        &self.valkey
    }

    /// Register a connection of `account`, opened for the Propolis session `sid`.
    pub fn register(
        &self,
        account: Uuid,
        sid: Option<String>,
    ) -> (ConnId, mpsc::Receiver<ClientFrame>) {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel(SEND_BUFFER);
        let mut guard = self.connections.lock().expect("fan-out registry poisoned");
        guard
            .entry(account)
            .or_default()
            .insert(id, Conn { sid, tx });
        (id, rx)
    }

    /// Forget a connection. True when it was the account's last one on this process.
    pub fn unregister(&self, account: Uuid, id: ConnId) -> bool {
        let mut guard = self.connections.lock().expect("fan-out registry poisoned");
        match guard.get_mut(&account) {
            Some(conns) => {
                conns.remove(&id);
                if conns.is_empty() {
                    guard.remove(&account);
                    return true;
                }
                false
            }
            None => true,
        }
    }

    /// Whether the account has a connection on this process.
    pub fn is_locally_connected(&self, account: Uuid) -> bool {
        let guard = self.connections.lock().expect("fan-out registry poisoned");
        guard.get(&account).is_some_and(|c| !c.is_empty())
    }

    async fn send(&self, wire: &Wire) {
        let payload = match serde_json::to_string(wire) {
            Ok(json) => json,
            Err(error) => {
                tracing::error!(%error, "cannot serialise a fan-out message");
                return;
            }
        };
        if let Err(error) = self
            .valkey
            .next()
            .publish::<(), _, _>(CHANNEL, payload)
            .await
        {
            tracing::warn!(%error, "cannot publish on the fan-out channel");
        }
    }

    /// Deliver `frame` to every connection of `audience`, on every process.
    pub async fn publish(&self, audience: Vec<Uuid>, frame: ClientFrame) {
        if !audience.is_empty() {
            self.send(&Wire::Frame { audience, frame }).await;
        }
    }

    /// Close what a Propolis session opened, on every process.
    pub async fn close_session(&self, sid: &str) {
        self.send(&Wire::Close {
            sid: Some(sid.to_owned()),
            account: None,
        })
        .await;
    }

    /// Close every connection of an account, on every process.
    pub async fn close_account(&self, account: Uuid) {
        self.send(&Wire::Close {
            sid: None,
            account: Some(account),
        })
        .await;
    }

    /// Act on a message of the channel, for this process's connections. Dropping a connection's
    /// sender ends its socket or stream, which then unregisters itself.
    fn deliver(&self, wire: Wire) {
        let mut guard = self.connections.lock().expect("fan-out registry poisoned");
        match wire {
            Wire::Frame { audience, frame } => {
                for account in &audience {
                    for conn in guard.get(account).into_iter().flat_map(|c| c.values()) {
                        if let Err(mpsc::error::TrySendError::Full(_)) =
                            conn.tx.try_send(frame.clone())
                        {
                            tracing::debug!(%account, "a connection's buffer is full; one frame lost");
                        }
                    }
                }
            }
            Wire::Close { sid, account } => {
                guard.retain(|owner, conns| {
                    conns.retain(|_, conn| {
                        let by_session = sid.is_some() && conn.sid == sid;
                        let by_account = account == Some(*owner);
                        !(by_session || by_account)
                    });
                    !conns.is_empty()
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fanout() -> Arc<Fanout> {
        let valkey = crate::cache::builder("redis://127.0.0.1:1")
            .unwrap()
            .build_pool(1)
            .unwrap();
        Fanout::detached(valkey)
    }

    #[tokio::test]
    async fn a_frame_reaches_its_audience_alone() {
        let fanout = fanout();
        let (pat, sam) = (Uuid::new_v4(), Uuid::new_v4());
        let (_, mut to_pat) = fanout.register(pat, None);
        let (_, mut to_sam) = fanout.register(sam, None);
        let frame =
            ClientFrame::new("chat.message.created", serde_json::json!({ "id": 1 })).unwrap();
        fanout.deliver(Wire::Frame {
            audience: vec![pat],
            frame: frame.clone(),
        });
        assert_eq!(to_pat.try_recv().unwrap(), frame);
        assert!(to_sam.try_recv().is_err());
    }

    #[tokio::test]
    async fn closing_a_session_ends_its_connections_and_no_other() {
        let fanout = fanout();
        let pat = Uuid::new_v4();
        let (_, mut laptop) = fanout.register(pat, Some("s1".into()));
        let (_, mut phone) = fanout.register(pat, Some("s2".into()));
        fanout.deliver(Wire::Close {
            sid: Some("s1".into()),
            account: None,
        });
        assert!(matches!(
            laptop.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
        assert!(matches!(
            phone.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        fanout.deliver(Wire::Close {
            sid: None,
            account: Some(pat),
        });
        assert!(matches!(
            phone.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
        assert!(!fanout.is_locally_connected(pat));
    }
}

//! In-process realtime fan-out. Every WebSocket connection registers under an actor id
//! (user id or guest id); services publish events to sets of actors. Events are serialized
//! once and shared as `Arc<str>` across all receivers, so a message to a 50 person room costs
//! one JSON encode and 50 pointer copies.

use dashmap::DashMap;
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::mpsc;

/// Outgoing buffer per connection. A client that falls this far behind is dropped and
/// resynchronizes over the REST API when it reconnects, instead of growing server memory.
const CONN_BUFFER: usize = 512;

pub type EventRx = mpsc::Receiver<Arc<str>>;
type Connections = Vec<(u64, mpsc::Sender<Arc<str>>)>;

#[derive(Default)]
pub struct Hub {
    conns: DashMap<String, Connections>,
    next_id: AtomicU64,
}

impl Hub {
    pub fn connect(&self, actor: &str) -> (u64, EventRx) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel(CONN_BUFFER);
        self.conns.entry(actor.to_string()).or_default().push((id, tx));
        (id, rx)
    }

    pub fn disconnect(&self, actor: &str, conn: u64) {
        let empty = match self.conns.get_mut(actor) {
            Some(mut list) => {
                list.retain(|(id, _)| *id != conn);
                list.is_empty()
            }
            None => false,
        };
        if empty {
            self.conns.remove_if(actor, |_, l| l.is_empty());
        }
    }

    pub fn is_online(&self, actor: &str) -> bool {
        self.conns.get(actor).is_some_and(|l| !l.is_empty())
    }

    pub fn online_count(&self) -> usize {
        self.conns.len()
    }

    /// Sends `event` to every connection of every actor in `actors`.
    pub fn publish<'a>(&self, actors: impl IntoIterator<Item = &'a str>, event: &Value) {
        let payload: Arc<str> = Arc::from(event.to_string());
        for actor in actors {
            self.send_raw(actor, &payload, None);
        }
    }

    /// Sends to one actor, optionally only to a single connection of theirs.
    pub fn send_raw(&self, actor: &str, payload: &Arc<str>, only_conn: Option<u64>) {
        let mut dead = Vec::new();
        if let Some(list) = self.conns.get(actor) {
            for (id, tx) in list.iter() {
                if only_conn.is_some_and(|c| c != *id) {
                    continue;
                }
                if tx.try_send(payload.clone()).is_err() {
                    dead.push(*id);
                }
            }
        }
        for id in dead {
            self.disconnect(actor, id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn fan_out_reaches_all_connections() {
        let hub = Hub::default();
        let (_a1, mut r1) = hub.connect("alice");
        let (_a2, mut r2) = hub.connect("alice");
        let (b, mut r3) = hub.connect("bob");
        hub.publish(["alice", "bob"], &json!({"type": "ping"}));
        assert_eq!(&*r1.recv().await.unwrap(), r#"{"type":"ping"}"#);
        assert_eq!(&*r2.recv().await.unwrap(), r#"{"type":"ping"}"#);
        assert_eq!(&*r3.recv().await.unwrap(), r#"{"type":"ping"}"#);
        hub.disconnect("bob", b);
        assert!(!hub.is_online("bob"));
        assert!(hub.is_online("alice"));
    }
}

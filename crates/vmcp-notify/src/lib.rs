//! In-memory notification bus.
//!
//! Replaces Python `events/streams.py` (which used Redis Streams). No external
//! dependency — `tokio::sync::broadcast` for live subscribers + a `VecDeque`
//! ring buffer for replay-from-cursor. Durability across restarts is
//! intentionally dropped (the explicit Redis-replacement trade-off).
//!
//! Wire model: upstream MCP servers emit JSON-RPC `notifications/*` frames;
//! `vmcp-upstream` forwards them to [`Bus::publish`]; the GraphQL resolver for
//! the `notifications` root field calls [`Bus::replay_since`]; long-lived
//! subscribers (when we add SSE later) tap into [`Bus::subscribe`].

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use serde::Serialize;
use tokio::sync::broadcast;

/// A server-initiated MCP notification, normalized for in-process fan-out.
#[derive(Clone, Debug, Serialize)]
pub struct Notification {
    /// Monotonically increasing ID. Use as the `replay_since` cursor.
    pub id: u64,
    /// Upstream server that emitted the notification (logical name from registry).
    pub source: String,
    /// JSON-RPC `method`, e.g. `"notifications/tools/list_changed"`.
    pub method: String,
    /// JSON-RPC `params` payload, opaque to the bus.
    pub params: serde_json::Value,
    /// Wall-clock timestamp in milliseconds since the unix epoch.
    pub ts_unix_ms: i64,
}

/// Default broadcast channel capacity. Live subscribers will get
/// `RecvError::Lagged` if they fall this far behind — the ring buffer remains
/// the durable replay source.
const DEFAULT_BROADCAST_CAPACITY: usize = 1024;

/// In-process notification fan-out + ring buffer for replay.
pub struct Bus {
    tx: broadcast::Sender<Arc<Notification>>,
    ring: Mutex<VecDeque<Arc<Notification>>>,
    ring_cap: usize,
    next_id: AtomicU64,
}

impl Bus {
    /// Construct a new bus with the given ring buffer capacity (MAXLEN).
    pub fn new(ring_cap: usize) -> Arc<Self> {
        let (tx, _) = broadcast::channel(DEFAULT_BROADCAST_CAPACITY);
        Arc::new(Self {
            tx,
            ring: Mutex::new(VecDeque::with_capacity(ring_cap.min(1024))),
            ring_cap,
            next_id: AtomicU64::new(1),
        })
    }

    /// Publish a notification. Always succeeds — the broadcast send errors when
    /// no subscribers are connected, which is fine; the ring is the durable
    /// copy.
    pub fn publish(&self, source: impl Into<String>, method: impl Into<String>, params: serde_json::Value) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let n = Arc::new(Notification {
            id,
            source: source.into(),
            method: method.into(),
            params,
            ts_unix_ms: chrono::Utc::now().timestamp_millis(),
        });

        // Update the ring buffer first — it's the durable source.
        {
            let mut ring = self.ring.lock();
            if ring.len() == self.ring_cap {
                ring.pop_front();
            }
            ring.push_back(Arc::clone(&n));
        }

        // Then fan out to live subscribers. Err means no receivers — discard.
        let _ = self.tx.send(n);
    }

    /// Subscribe for live notifications. Each receiver gets every publish that
    /// happens after subscription. Falling behind by more than the broadcast
    /// capacity yields `RecvError::Lagged` — the caller should resync via
    /// `replay_since`.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Notification>> {
        self.tx.subscribe()
    }

    /// Return all buffered notifications with `id > cursor`, up to `limit`.
    /// `cursor == 0` returns from the start of the ring.
    pub fn replay_since(&self, cursor: u64, limit: usize) -> Vec<Arc<Notification>> {
        let ring = self.ring.lock();
        ring.iter()
            .filter(|n| n.id > cursor)
            .take(limit)
            .cloned()
            .collect()
    }

    /// Current monotonically-issued ID (next publish gets this value).
    pub fn next_id_hint(&self) -> u64 {
        self.next_id.load(Ordering::Relaxed)
    }

    /// Current ring buffer length (for metrics / diagnostics).
    pub fn ring_len(&self) -> usize {
        self.ring.lock().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn publish_assigns_monotonic_ids() {
        let bus = Bus::new(16);
        bus.publish("a", "notifications/tools/list_changed", json!({}));
        bus.publish("a", "notifications/tools/list_changed", json!({}));
        bus.publish("b", "notifications/progress", json!({"value": 1}));

        let all = bus.replay_since(0, 100);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].id, 1);
        assert_eq!(all[1].id, 2);
        assert_eq!(all[2].id, 3);
        assert_eq!(all[2].source, "b");
    }

    #[test]
    fn ring_buffer_evicts_oldest() {
        let bus = Bus::new(2);
        for i in 0..5 {
            bus.publish("x", "notifications/test", json!({"i": i}));
        }
        let all = bus.replay_since(0, 100);
        assert_eq!(all.len(), 2, "ring buffer should hold at most ring_cap");
        // We kept the last 2 — IDs 4 and 5.
        assert_eq!(all[0].id, 4);
        assert_eq!(all[1].id, 5);
    }

    #[test]
    fn replay_since_filters_by_cursor() {
        let bus = Bus::new(16);
        for _ in 0..5 {
            bus.publish("x", "notifications/test", json!({}));
        }
        // Cursor=2 means: give me everything strictly after id 2.
        let after = bus.replay_since(2, 100);
        assert_eq!(after.len(), 3);
        assert_eq!(after[0].id, 3);
    }

    #[test]
    fn replay_since_respects_limit() {
        let bus = Bus::new(16);
        for _ in 0..5 {
            bus.publish("x", "notifications/test", json!({}));
        }
        let limited = bus.replay_since(0, 2);
        assert_eq!(limited.len(), 2);
    }

    #[tokio::test]
    async fn subscribers_receive_live_notifications() {
        let bus = Bus::new(16);
        let mut rx = bus.subscribe();
        bus.publish("u", "notifications/tools/list_changed", json!({"tool": "x"}));
        let got = rx.recv().await.expect("should receive");
        assert_eq!(got.source, "u");
        assert_eq!(got.id, 1);
    }
}

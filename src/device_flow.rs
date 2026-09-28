//! In-memory state for the Codex device-code sign-in flow served by
//! `GET /auth`. A device code sits waiting for minutes and is polled
//! repeatedly from the browser, unlike everything else this binary
//! does, which is stateless request-in/request-out — hence a small
//! dedicated store rather than folding this into `App` directly.
//!
//! Client-driven polling: the browser calls `POST /auth/device/poll` on
//! its own timer, but a call only reaches auth.openai.com once
//! `next_poll_at` has passed. That keeps a tab polling faster than the
//! vendor's own interval, or a second tab on the same flow, from
//! multiplying upstream polls — the flow's own pace governs regardless
//! of how often the client asks.

use std::collections::HashMap;
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::time::Instant;

use codex_bridge::auth::DeviceCode;

/// `Completing` covers the seconds between the vendor issuing a grant
/// and the account being installed (token exchange, file write). A poll
/// landing then must read back as still-pending, not find the flow gone
/// and tell the browser its code expired while the sign-in is about to
/// succeed. `Connected` is kept until the flow's own expiry so a later
/// poll — a second tab, a tick already in flight — hears the same
/// answer the first one did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Polling,
    Completing,
    Connected,
}

#[derive(Clone)]
pub struct DeviceFlowState {
    pub device_auth_id: String,
    pub user_code: String,
    pub interval: Duration,
    pub expires_at: Instant,
    pub next_poll_at: Instant,
    pub phase: Phase,
}

// Mirrors codex-rs's own device-auth ceiling.
const DEVICE_CODE_TTL: Duration = Duration::from_secs(15 * 60);

pub struct DeviceFlowStore {
    flows: Mutex<HashMap<String, DeviceFlowState>>,
}

impl DeviceFlowStore {
    pub fn new() -> Self {
        Self {
            flows: Mutex::new(HashMap::new()),
        }
    }

    /// Register a freshly issued device code and return its flow id and
    /// absolute expiry. A flow is only ever removed when a poll finds it
    /// finished or expired, so a browser tab closed mid sign-in would
    /// otherwise leak its entry for the life of the process — starting a
    /// new flow is the natural moment to sweep those out.
    pub async fn create(&self, code: &DeviceCode) -> (String, Instant) {
        let now = Instant::now();
        let expires_at = now + DEVICE_CODE_TTL;
        let mut flows = self.flows.lock().await;
        // A flow mid-exchange is left for the request finishing it.
        flows.retain(|_, flow| now < flow.expires_at || flow.phase == Phase::Completing);
        let flow_id = flow_id();
        flows.insert(
            flow_id.clone(),
            DeviceFlowState {
                device_auth_id: code.device_auth_id.clone(),
                user_code: code.user_code.clone(),
                interval: code.interval,
                expires_at,
                // The first poll is allowed immediately — the interval
                // only throttles polls AFTER the vendor has answered once.
                next_poll_at: now,
                phase: Phase::Polling,
            },
        );
        (flow_id, expires_at)
    }

    pub async fn get(&self, flow_id: &str) -> Option<DeviceFlowState> {
        self.flows.lock().await.get(flow_id).cloned()
    }

    pub async fn delete(&self, flow_id: &str) {
        self.flows.lock().await.remove(flow_id);
    }

    /// Record that an upstream poll just happened, so the next one waits
    /// out the flow's own interval.
    pub async fn mark_polled(&self, flow_id: &str) {
        if let Some(flow) = self.flows.lock().await.get_mut(flow_id) {
            flow.next_poll_at = Instant::now() + flow.interval;
        }
    }

    pub async fn set_phase(&self, flow_id: &str, phase: Phase) {
        if let Some(flow) = self.flows.lock().await.get_mut(flow_id) {
            flow.phase = phase;
        }
    }
}

impl Default for DeviceFlowStore {
    fn default() -> Self {
        Self::new()
    }
}

fn flow_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code() -> DeviceCode {
        DeviceCode {
            device_auth_id: "da-1".into(),
            user_code: "ABCD-1234".into(),
            verification_uri: "https://auth.openai.com/codex/device".into(),
            interval: Duration::from_secs(5),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_fresh_flow_is_immediately_pollable_and_not_expired() {
        let store = DeviceFlowStore::new();
        let (flow_id, expires_at) = store.create(&code()).await;
        let flow = store.get(&flow_id).await.unwrap();
        assert_eq!(flow.phase, Phase::Polling);
        assert_eq!(flow.next_poll_at, Instant::now());
        assert_eq!(flow.expires_at, expires_at);
        assert!(Instant::now() < expires_at);
    }

    #[tokio::test(start_paused = true)]
    async fn mark_polled_throttles_the_next_poll_by_the_flows_interval() {
        let store = DeviceFlowStore::new();
        let (flow_id, _) = store.create(&code()).await;
        store.mark_polled(&flow_id).await;
        let flow = store.get(&flow_id).await.unwrap();
        assert_eq!(flow.next_poll_at, Instant::now() + Duration::from_secs(5));
    }

    #[tokio::test(start_paused = true)]
    async fn set_phase_updates_only_the_named_flow() {
        let store = DeviceFlowStore::new();
        let (a, _) = store.create(&code()).await;
        let (b, _) = store.create(&code()).await;
        store.set_phase(&a, Phase::Connected).await;
        assert_eq!(store.get(&a).await.unwrap().phase, Phase::Connected);
        assert_eq!(store.get(&b).await.unwrap().phase, Phase::Polling);
    }

    #[tokio::test(start_paused = true)]
    async fn delete_removes_the_flow() {
        let store = DeviceFlowStore::new();
        let (flow_id, _) = store.create(&code()).await;
        store.delete(&flow_id).await;
        assert!(store.get(&flow_id).await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn get_of_an_unknown_flow_is_none() {
        let store = DeviceFlowStore::new();
        assert!(store.get("no-such-flow").await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn creating_a_new_flow_sweeps_expired_ones_but_keeps_completing_ones() {
        let store = DeviceFlowStore::new();
        let (expired, _) = store.create(&code()).await;
        let (completing, _) = store.create(&code()).await;
        store.set_phase(&completing, Phase::Completing).await;

        tokio::time::advance(DEVICE_CODE_TTL + Duration::from_secs(1)).await;
        store.create(&code()).await;

        assert!(store.get(&expired).await.is_none());
        assert!(store.get(&completing).await.is_some());
    }
}

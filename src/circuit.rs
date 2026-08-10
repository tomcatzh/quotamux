use std::time::Duration;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::{store::Store, types::FailureClass};

const CIRCUIT_KEY: &str = "opencode-go-circuit";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CircuitMode {
    Closed,
    Open,
    HalfOpen,
    Suspended,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CircuitSnapshot {
    pub mode: CircuitMode,
    pub reason: Option<FailureClass>,
    pub opened_at_ms: Option<i64>,
    pub next_probe_at_ms: Option<i64>,
    pub consecutive_failures: u32,
    pub backoff_level: usize,
    pub probe_in_flight: bool,
}

impl Default for CircuitSnapshot {
    fn default() -> Self {
        Self {
            mode: CircuitMode::Closed,
            reason: None,
            opened_at_ms: None,
            next_probe_at_ms: None,
            consecutive_failures: 0,
            backoff_level: 0,
            probe_in_flight: false,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RouteDecision {
    Primary { probe: bool },
    Fallback { reason: Option<FailureClass> },
}

pub struct CircuitBreaker {
    state: Mutex<CircuitSnapshot>,
    store: Store,
}

impl CircuitBreaker {
    pub fn load(store: Store) -> Result<Self, String> {
        let mut state = store
            .get_state::<CircuitSnapshot>(CIRCUIT_KEY)?
            .unwrap_or_default();
        if state.mode == CircuitMode::HalfOpen {
            state.mode = CircuitMode::Open;
            state.probe_in_flight = false;
        }
        Ok(Self {
            state: Mutex::new(state),
            store,
        })
    }

    pub async fn decide(&self) -> RouteDecision {
        let mut state = self.state.lock().await;
        let now = Utc::now().timestamp_millis();
        match state.mode {
            CircuitMode::Closed => RouteDecision::Primary { probe: false },
            CircuitMode::Open | CircuitMode::Suspended
                if state
                    .next_probe_at_ms
                    .is_some_and(|deadline| deadline <= now)
                    && !state.probe_in_flight =>
            {
                state.mode = CircuitMode::HalfOpen;
                state.probe_in_flight = true;
                self.persist(&state);
                RouteDecision::Primary { probe: true }
            }
            _ => RouteDecision::Fallback {
                reason: state.reason,
            },
        }
    }

    pub async fn success(&self) {
        let mut state = self.state.lock().await;
        *state = CircuitSnapshot::default();
        self.persist(&state);
    }

    pub async fn failure(&self, class: FailureClass, retry_after: Option<Duration>) {
        if matches!(
            class,
            FailureClass::ClientRequest | FailureClass::ClientCancelled
        ) {
            let mut state = self.state.lock().await;
            state.probe_in_flight = false;
            if state.mode == CircuitMode::HalfOpen {
                state.mode = CircuitMode::Open;
            }
            self.persist(&state);
            return;
        }

        let mut state = self.state.lock().await;
        let now = Utc::now().timestamp_millis();
        let same_reason = state.reason == Some(class);
        if !same_reason {
            state.backoff_level = 0;
        }
        let schedule = schedule_for(class);
        let delay = retry_after
            .unwrap_or_else(|| schedule[state.backoff_level.min(schedule.len().saturating_sub(1))]);
        state.mode = if matches!(
            class,
            FailureClass::ProviderAuth
                | FailureClass::ProviderBilling
                | FailureClass::ProviderConfiguration
                | FailureClass::ProviderUnknown4xx
        ) {
            CircuitMode::Suspended
        } else {
            CircuitMode::Open
        };
        state.reason = Some(class);
        state.opened_at_ms.get_or_insert(now);
        state.next_probe_at_ms = Some(now + delay.as_millis().min(i64::MAX as u128) as i64);
        state.consecutive_failures = state.consecutive_failures.saturating_add(1);
        state.backoff_level = state.backoff_level.saturating_add(1);
        state.probe_in_flight = false;
        self.persist(&state);
    }

    pub async fn snapshot(&self) -> CircuitSnapshot {
        self.state.lock().await.clone()
    }

    fn persist(&self, state: &CircuitSnapshot) {
        if let Err(error) = self.store.put_state(CIRCUIT_KEY, state) {
            tracing::error!(%error, "failed to persist circuit state");
        }
    }
}

fn schedule_for(class: FailureClass) -> &'static [Duration] {
    const TRANSIENT: &[Duration] = &[
        Duration::from_secs(1),
        Duration::from_secs(2),
        Duration::from_secs(4),
        Duration::from_secs(8),
        Duration::from_secs(16),
        Duration::from_secs(32),
        Duration::from_secs(60),
    ];
    const CAPACITY: &[Duration] = &[
        Duration::from_secs(5),
        Duration::from_secs(30),
        Duration::from_secs(300),
    ];
    const SLOW: &[Duration] = &[
        Duration::from_secs(300),
        Duration::from_secs(1800),
        Duration::from_secs(3600),
    ];
    const BILLING: &[Duration] = &[Duration::from_secs(3600)];
    match class {
        FailureClass::ProviderCapacity => CAPACITY,
        FailureClass::ProviderBilling => BILLING,
        FailureClass::ProviderAuth
        | FailureClass::ProviderConfiguration
        | FailureClass::ProviderUnknown4xx => SLOW,
        _ => TRANSIENT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn transient_failures_use_exponential_backoff() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let circuit = CircuitBreaker::load(store).unwrap();
        circuit.failure(FailureClass::ProviderTransient, None).await;
        let first = circuit.snapshot().await;
        assert_eq!(first.mode, CircuitMode::Open);
        assert_eq!(first.backoff_level, 1);
        assert!(first.next_probe_at_ms.unwrap() - Utc::now().timestamp_millis() <= 1_100);
    }

    #[tokio::test]
    async fn auth_failure_suspends_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let circuit = CircuitBreaker::load(store).unwrap();
        circuit.failure(FailureClass::ProviderAuth, None).await;
        assert_eq!(circuit.snapshot().await.mode, CircuitMode::Suspended);
    }

    #[tokio::test]
    async fn success_resets_circuit() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let circuit = CircuitBreaker::load(store).unwrap();
        circuit.failure(FailureClass::ProviderTransient, None).await;
        circuit.success().await;
        assert_eq!(circuit.snapshot().await.mode, CircuitMode::Closed);
    }
}

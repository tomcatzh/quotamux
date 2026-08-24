use std::{sync::Arc, time::Duration};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::{store::Store, types::FailureClass};

const ABANDONED_PROBE_RETRY: Duration = Duration::from_secs(1);
const PROBE_LEASE: Duration = Duration::from_secs(10 * 60);

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
    #[serde(default)]
    pub probe_deadline_at_ms: Option<i64>,
    #[serde(default)]
    pub generation: u64,
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
            probe_deadline_at_ms: None,
            generation: 0,
        }
    }
}

pub enum RouteDecision {
    Primary { permit: CircuitPermit },
    Fallback { reason: Option<FailureClass> },
}

pub struct CircuitPermit {
    circuit: Arc<CircuitBreaker>,
    generation: u64,
    probe: bool,
    settled: bool,
}

impl CircuitPermit {
    pub const fn is_probe(&self) -> bool {
        self.probe
    }

    pub async fn success(mut self) {
        self.circuit
            .complete_success(self.generation, self.probe)
            .await;
        self.settled = true;
    }

    pub async fn failure(mut self, class: FailureClass, retry_after: Option<Duration>) {
        self.circuit
            .complete_failure(self.generation, self.probe, class, retry_after)
            .await;
        self.settled = true;
    }

    pub async fn abandon(mut self) {
        self.circuit
            .complete_abandonment(self.generation, self.probe)
            .await;
        self.settled = true;
    }
}

impl Drop for CircuitPermit {
    fn drop(&mut self) {
        if self.settled || !self.probe {
            return;
        }
        let circuit = self.circuit.clone();
        let generation = self.generation;
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    circuit.complete_abandonment(generation, true).await;
                });
            }
            Err(error) => {
                tracing::error!(%error, "could not release abandoned circuit probe");
            }
        }
    }
}

pub struct CircuitBreaker {
    state: Mutex<CircuitSnapshot>,
    store: Store,
    store_key: String,
}

impl CircuitBreaker {
    pub fn load(store: Store, store_key: impl Into<String>) -> Result<Self, String> {
        let store_key = store_key.into();
        let mut state = store
            .get_state::<CircuitSnapshot>(&store_key)?
            .unwrap_or_default();
        let now = Utc::now().timestamp_millis();
        let mut changed = false;
        if state.mode == CircuitMode::HalfOpen {
            state.mode = unavailable_mode(state.reason);
            state.probe_in_flight = false;
            state.probe_deadline_at_ms = None;
            state.next_probe_at_ms = Some(now);
            state.generation = next_generation(state.generation);
            changed = true;
        } else if matches!(state.mode, CircuitMode::Open | CircuitMode::Suspended) {
            let class = state
                .reason
                .unwrap_or(FailureClass::ProviderUnknown5xxOrTransport);
            let maximum_deadline = add_duration_ms(now, maximum_retry_delay(class));
            if state
                .next_probe_at_ms
                .is_some_and(|deadline| deadline > maximum_deadline)
            {
                state.next_probe_at_ms = Some(maximum_deadline);
                changed = true;
            }
        }
        if changed {
            store.put_state(&store_key, &state)?;
        }
        Ok(Self {
            state: Mutex::new(state),
            store,
            store_key,
        })
    }

    pub async fn decide(self: &Arc<Self>) -> RouteDecision {
        self.decide_at(Utc::now().timestamp_millis()).await
    }

    async fn decide_at(self: &Arc<Self>, now: i64) -> RouteDecision {
        let mut state = self.state.lock().await;
        if state.mode == CircuitMode::HalfOpen
            && state
                .probe_deadline_at_ms
                .is_some_and(|deadline| deadline <= now)
        {
            state.mode = unavailable_mode(state.reason);
            state.probe_in_flight = false;
            state.probe_deadline_at_ms = None;
            state.next_probe_at_ms = Some(now);
            state.generation = next_generation(state.generation);
        }

        match state.mode {
            CircuitMode::Closed => RouteDecision::Primary {
                permit: CircuitPermit {
                    circuit: self.clone(),
                    generation: state.generation,
                    probe: false,
                    settled: false,
                },
            },
            CircuitMode::Open | CircuitMode::Suspended
                if state
                    .next_probe_at_ms
                    .is_some_and(|deadline| deadline <= now) =>
            {
                state.mode = CircuitMode::HalfOpen;
                state.probe_in_flight = true;
                state.probe_deadline_at_ms = Some(add_duration_ms(now, PROBE_LEASE));
                self.persist(&state);
                RouteDecision::Primary {
                    permit: CircuitPermit {
                        circuit: self.clone(),
                        generation: state.generation,
                        probe: true,
                        settled: false,
                    },
                }
            }
            _ => RouteDecision::Fallback {
                reason: state.reason,
            },
        }
    }

    async fn complete_success(&self, generation: u64, probe: bool) {
        let mut state = self.state.lock().await;
        if generation != state.generation {
            return;
        }
        if !probe && state.mode == CircuitMode::Closed {
            return;
        }
        if probe && state.mode != CircuitMode::HalfOpen {
            return;
        }
        let generation = next_generation(state.generation);
        *state = CircuitSnapshot {
            generation,
            ..CircuitSnapshot::default()
        };
        self.persist(&state);
    }

    async fn complete_failure(
        &self,
        generation: u64,
        probe: bool,
        class: FailureClass,
        retry_after: Option<Duration>,
    ) {
        if !class.affects_circuit() {
            self.complete_abandonment(generation, probe).await;
            return;
        }

        let mut state = self.state.lock().await;
        if generation != state.generation {
            return;
        }
        if (!probe && state.mode != CircuitMode::Closed)
            || (probe && state.mode != CircuitMode::HalfOpen)
        {
            return;
        }

        let now = Utc::now().timestamp_millis();
        if state.reason != Some(class) {
            state.backoff_level = 0;
        }
        let delay = retry_delay(class, state.backoff_level, retry_after);
        state.mode = unavailable_mode(Some(class));
        state.reason = Some(class);
        state.opened_at_ms.get_or_insert(now);
        state.next_probe_at_ms = Some(add_duration_ms(now, delay));
        state.consecutive_failures = state.consecutive_failures.saturating_add(1);
        state.backoff_level = state.backoff_level.saturating_add(1);
        state.probe_in_flight = false;
        state.probe_deadline_at_ms = None;
        state.generation = next_generation(state.generation);
        self.persist(&state);
    }

    async fn complete_abandonment(&self, generation: u64, probe: bool) {
        if !probe {
            return;
        }
        let mut state = self.state.lock().await;
        if generation != state.generation || state.mode != CircuitMode::HalfOpen {
            return;
        }
        let now = Utc::now().timestamp_millis();
        state.mode = unavailable_mode(state.reason);
        state.next_probe_at_ms = Some(add_duration_ms(now, ABANDONED_PROBE_RETRY));
        state.probe_in_flight = false;
        state.probe_deadline_at_ms = None;
        state.generation = next_generation(state.generation);
        self.persist(&state);
    }

    pub async fn snapshot(&self) -> CircuitSnapshot {
        self.state.lock().await.clone()
    }

    fn persist(&self, state: &CircuitSnapshot) {
        if let Err(error) = self.store.put_state(&self.store_key, state) {
            tracing::error!(%error, "failed to persist circuit state");
        }
    }
}

fn next_generation(generation: u64) -> u64 {
    generation.wrapping_add(1)
}

fn add_duration_ms(now: i64, delay: Duration) -> i64 {
    now.saturating_add(delay.as_millis().min(i64::MAX as u128) as i64)
}

fn unavailable_mode(reason: Option<FailureClass>) -> CircuitMode {
    if matches!(
        reason,
        Some(
            FailureClass::ProviderAuth
                | FailureClass::ProviderBilling
                | FailureClass::ProviderConfiguration
                | FailureClass::ProviderUnknown4xx
        )
    ) {
        CircuitMode::Suspended
    } else {
        CircuitMode::Open
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
    const QUOTA: &[Duration] = &[
        Duration::from_secs(60),
        Duration::from_secs(300),
        Duration::from_secs(900),
    ];
    const BILLING: &[Duration] = &[Duration::from_secs(3600)];
    match class {
        FailureClass::ProviderCapacity => CAPACITY,
        FailureClass::ProviderQuota => QUOTA,
        FailureClass::ProviderBilling => BILLING,
        FailureClass::ProviderAuth
        | FailureClass::ProviderConfiguration
        | FailureClass::ProviderUnknown4xx => SLOW,
        _ => TRANSIENT,
    }
}

fn retry_delay(
    class: FailureClass,
    backoff_level: usize,
    retry_after: Option<Duration>,
) -> Duration {
    let schedule = schedule_for(class);
    let scheduled = schedule[backoff_level.min(schedule.len().saturating_sub(1))];
    if !matches!(
        class,
        FailureClass::ProviderCapacity
            | FailureClass::ProviderQuota
            | FailureClass::ProviderTransient
            | FailureClass::StreamFailure
            | FailureClass::ProviderUnknown5xxOrTransport
    ) {
        return scheduled;
    }
    retry_after
        .map(|delay| delay.min(maximum_retry_delay(class)))
        .unwrap_or(scheduled)
}

fn maximum_retry_delay(class: FailureClass) -> Duration {
    *schedule_for(class)
        .last()
        .expect("non-empty retry schedule")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn primary(decision: RouteDecision) -> CircuitPermit {
        match decision {
            RouteDecision::Primary { permit } => permit,
            RouteDecision::Fallback { reason } => panic!("unexpected fallback: {reason:?}"),
        }
    }

    async fn test_circuit() -> (tempfile::TempDir, Arc<CircuitBreaker>) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let circuit = Arc::new(CircuitBreaker::load(store, "test-circuit").unwrap());
        (dir, circuit)
    }

    #[tokio::test]
    async fn transient_failures_use_exponential_backoff() {
        let (_dir, circuit) = test_circuit().await;
        primary(circuit.decide().await)
            .failure(FailureClass::ProviderTransient, None)
            .await;
        let first = circuit.snapshot().await;
        assert_eq!(first.mode, CircuitMode::Open);
        assert_eq!(first.backoff_level, 1);
        assert!(first.next_probe_at_ms.unwrap() - Utc::now().timestamp_millis() <= 1_100);

        let probe = primary(circuit.decide_at(first.next_probe_at_ms.unwrap()).await);
        assert!(probe.is_probe());
        probe.failure(FailureClass::ProviderTransient, None).await;
        let second = circuit.snapshot().await;
        assert_eq!(second.backoff_level, 2);
        assert!(second.next_probe_at_ms.unwrap() - Utc::now().timestamp_millis() <= 2_100);
    }

    #[tokio::test]
    async fn auth_failure_suspends_immediately() {
        let (_dir, circuit) = test_circuit().await;
        primary(circuit.decide().await)
            .failure(FailureClass::ProviderAuth, None)
            .await;
        assert_eq!(circuit.snapshot().await.mode, CircuitMode::Suspended);
    }

    #[tokio::test]
    async fn ambiguous_provider_rejection_does_not_change_closed_circuit() {
        let (_dir, circuit) = test_circuit().await;
        primary(circuit.decide().await)
            .failure(FailureClass::ProviderAmbiguousRejection, None)
            .await;
        let state = circuit.snapshot().await;
        assert_eq!(state.mode, CircuitMode::Closed);
        assert_eq!(state.reason, None);
        assert_eq!(state.consecutive_failures, 0);
        assert_eq!(state.backoff_level, 0);
        assert_eq!(state.next_probe_at_ms, None);
    }

    #[tokio::test]
    async fn successful_probe_resets_the_circuit() {
        let (_dir, circuit) = test_circuit().await;
        primary(circuit.decide().await)
            .failure(FailureClass::ProviderTransient, Some(Duration::ZERO))
            .await;
        let probe = primary(circuit.decide().await);
        assert!(probe.is_probe());
        probe.success().await;
        assert_eq!(circuit.snapshot().await.mode, CircuitMode::Closed);
    }

    #[tokio::test]
    async fn concurrent_failures_from_one_closed_generation_count_once() {
        let (_dir, circuit) = test_circuit().await;
        let permits = futures_util::future::join_all((0..8).map(|_| circuit.decide())).await;
        let mut permits = permits.into_iter().map(primary);
        permits
            .next()
            .unwrap()
            .failure(FailureClass::ProviderCapacity, None)
            .await;
        for permit in permits {
            permit.failure(FailureClass::ProviderCapacity, None).await;
        }
        let state = circuit.snapshot().await;
        assert_eq!(state.consecutive_failures, 1);
        assert_eq!(state.backoff_level, 1);
    }

    #[tokio::test]
    async fn stale_success_cannot_close_a_new_failure_generation() {
        let (_dir, circuit) = test_circuit().await;
        let failing = primary(circuit.decide().await);
        let stale_success = primary(circuit.decide().await);
        failing.failure(FailureClass::ProviderTransient, None).await;
        stale_success.success().await;
        assert_eq!(circuit.snapshot().await.mode, CircuitMode::Open);
    }

    #[tokio::test]
    async fn dropped_half_open_permit_releases_the_probe() {
        let (_dir, circuit) = test_circuit().await;
        primary(circuit.decide().await)
            .failure(FailureClass::ProviderTransient, Some(Duration::ZERO))
            .await;
        let probe = primary(circuit.decide().await);
        assert!(probe.is_probe());
        drop(probe);

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let state = circuit.snapshot().await;
                if state.mode == CircuitMode::Open && !state.probe_in_flight {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("probe drop cleanup");
    }

    #[tokio::test]
    async fn expired_half_open_lease_is_reclaimed_and_stale_result_is_ignored() {
        let (_dir, circuit) = test_circuit().await;
        primary(circuit.decide().await)
            .failure(FailureClass::ProviderTransient, Some(Duration::ZERO))
            .await;
        let old_probe = primary(circuit.decide().await);
        let deadline = circuit
            .snapshot()
            .await
            .probe_deadline_at_ms
            .expect("probe deadline");

        let replacement = primary(circuit.decide_at(deadline).await);
        assert!(replacement.is_probe());
        old_probe.success().await;
        assert_eq!(circuit.snapshot().await.mode, CircuitMode::HalfOpen);
        replacement.abandon().await;
        assert_eq!(circuit.snapshot().await.mode, CircuitMode::Open);
    }

    #[test]
    fn retry_after_is_capped_by_failure_class_and_ignored_for_suspensions() {
        let huge = Some(Duration::from_secs(u64::MAX));
        assert_eq!(
            retry_delay(FailureClass::ProviderTransient, 0, huge),
            Duration::from_secs(60)
        );
        assert_eq!(
            retry_delay(FailureClass::ProviderCapacity, 0, huge),
            Duration::from_secs(300)
        );
        assert_eq!(
            retry_delay(FailureClass::ProviderQuota, 0, huge),
            Duration::from_secs(900)
        );
        assert_eq!(
            retry_delay(FailureClass::ProviderAuth, 0, huge),
            Duration::from_secs(300)
        );
        assert_eq!(
            retry_delay(FailureClass::ProviderCapacity, 2, Some(Duration::ZERO)),
            Duration::ZERO
        );
    }

    #[test]
    fn deadline_arithmetic_saturates() {
        assert_eq!(
            add_duration_ms(i64::MAX - 1, Duration::from_secs(u64::MAX)),
            i64::MAX
        );
    }

    #[tokio::test]
    async fn legacy_half_open_state_is_released_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store
            .put_state(
                "test-circuit",
                &serde_json::json!({
                    "mode":"half-open",
                    "reason":"provider_transient",
                    "opened_at_ms":1,
                    "next_probe_at_ms":1,
                    "consecutive_failures":1,
                    "backoff_level":1,
                    "probe_in_flight":true
                }),
            )
            .unwrap();

        let circuit = Arc::new(CircuitBreaker::load(store, "test-circuit").unwrap());
        let state = circuit.snapshot().await;
        assert_eq!(state.mode, CircuitMode::Open);
        assert!(!state.probe_in_flight);
        assert_eq!(state.probe_deadline_at_ms, None);
        assert_eq!(state.generation, 1);
        let probe = primary(circuit.decide().await);
        assert!(probe.is_probe());
        probe.abandon().await;
    }

    #[tokio::test]
    async fn capped_retry_deadline_survives_reload() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let circuit = Arc::new(CircuitBreaker::load(store.clone(), "test-circuit").unwrap());
        primary(circuit.decide().await)
            .failure(
                FailureClass::ProviderQuota,
                Some(Duration::from_secs(u64::MAX)),
            )
            .await;
        let before = circuit.snapshot().await;
        drop(circuit);

        let reloaded = CircuitBreaker::load(store, "test-circuit").unwrap();
        let after = reloaded.snapshot().await;
        assert_eq!(after.mode, CircuitMode::Open);
        assert_eq!(after.reason, Some(FailureClass::ProviderQuota));
        assert_eq!(after.next_probe_at_ms, before.next_probe_at_ms);
        assert!(
            after.next_probe_at_ms.unwrap() - Utc::now().timestamp_millis()
                <= Duration::from_secs(900).as_millis() as i64
        );
    }

    #[tokio::test]
    async fn legacy_uncapped_retry_deadline_is_clamped_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let now = Utc::now().timestamp_millis();
        store
            .put_state(
                "test-circuit",
                &CircuitSnapshot {
                    mode: CircuitMode::Open,
                    reason: Some(FailureClass::ProviderCapacity),
                    opened_at_ms: Some(now),
                    next_probe_at_ms: Some(add_duration_ms(
                        now,
                        Duration::from_secs(30 * 24 * 60 * 60),
                    )),
                    consecutive_failures: 1,
                    backoff_level: 1,
                    ..CircuitSnapshot::default()
                },
            )
            .unwrap();

        let circuit = CircuitBreaker::load(store.clone(), "test-circuit").unwrap();
        let loaded = circuit.snapshot().await;
        assert!(loaded.next_probe_at_ms.unwrap() - Utc::now().timestamp_millis() <= 300_000);
        let persisted = store
            .get_state::<CircuitSnapshot>("test-circuit")
            .unwrap()
            .unwrap();
        assert_eq!(persisted.next_probe_at_ms, loaded.next_probe_at_ms);
    }
}

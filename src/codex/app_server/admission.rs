//! Shared admission for this bot profile. This controls new turns, not native
//! tool-loop requests or other applications using the same upstream account.
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelAdmissionConfig {
    pub max_concurrent_turns: usize,
    pub start_interval_ms: u64,
    pub initial_backoff_seconds: u64,
    pub maximum_backoff_seconds: u64,
}

impl Default for ModelAdmissionConfig {
    fn default() -> Self {
        Self {
            max_concurrent_turns: 2,
            start_interval_ms: 1000,
            initial_backoff_seconds: 30,
            maximum_backoff_seconds: 300,
        }
    }
}

impl ModelAdmissionConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            (1..=8).contains(&self.max_concurrent_turns)
                && self.start_interval_ms <= 60_000
                && self.initial_backoff_seconds > 0
                && self.maximum_backoff_seconds >= self.initial_backoff_seconds
                && self.maximum_backoff_seconds <= 3600,
            "invalid_model_admission"
        );
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
#[error("model_admission_deferred")]
pub struct AdmissionDeferred;

#[derive(Clone, Debug, thiserror::Error)]
#[error("{kind}")]
pub struct ModelFailure {
    pub kind: &'static str,
    pub http_status: Option<u16>,
    pub native_code: &'static str,
}

impl ModelFailure {
    /// Only call on the native model error envelope, never tool output.
    pub(crate) fn from_native(error: &serde_json::Value) -> Option<Self> {
        let info = &error["codexErrorInfo"];
        let code = info.as_str().or_else(|| {
            info.as_object()
                .and_then(|v| v.keys().next().map(String::as_str))
        });
        if code == Some("usageLimitExceeded") {
            return Some(Self {
                kind: "model_usage_limit",
                http_status: None,
                native_code: "usageLimitExceeded",
            });
        }
        let status = info
            .get("httpStatusCode")
            .and_then(serde_json::Value::as_u64)
            .or_else(|| {
                info.as_object().and_then(|v| {
                    v.values()
                        .find_map(|v| v.get("httpStatusCode").and_then(serde_json::Value::as_u64))
                })
            });
        let message = error
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if code == Some("rateLimitExceeded")
            || status == Some(429)
            || message.contains("429 Too Many Requests")
            || message.contains("last status: 429")
        {
            Some(Self {
                kind: "model_rate_limited",
                http_status: Some(429),
                native_code: if code == Some("rateLimitExceeded") {
                    "rateLimitExceeded"
                } else {
                    "provider_reason_unavailable"
                },
            })
        } else {
            None
        }
    }
}

struct State {
    settings: ModelAdmissionConfig,
    active: usize,
    next_start: Instant,
    retry_at: Option<Instant>,
    strikes: u32,
    epoch: u64,
    probe_active: bool,
}

pub struct Admission(Mutex<State>);

impl Default for Admission {
    fn default() -> Self {
        Self(Mutex::new(State {
            settings: ModelAdmissionConfig {
                start_interval_ms: 0,
                max_concurrent_turns: 8,
                ..Default::default()
            },
            active: 0,
            next_start: Instant::now(),
            retry_at: None,
            strikes: 0,
            epoch: 0,
            probe_active: false,
        }))
    }
}

impl Admission {
    pub fn configure(&self, settings: ModelAdmissionConfig) -> anyhow::Result<()> {
        settings.validate()?;
        let mut state = self.0.lock().expect("admission lock");
        anyhow::ensure!(state.active == 0, "model_admission_busy");
        state.settings = settings;
        Ok(())
    }

    pub fn available(&self) -> bool {
        Self::ready(&self.0.lock().expect("admission lock"), Instant::now())
    }

    pub fn epoch(&self) -> u64 {
        self.0.lock().expect("admission lock").epoch
    }

    fn ready(state: &State, now: Instant) -> bool {
        now >= state.next_start
            && state.active < state.settings.max_concurrent_turns
            && state
                .retry_at
                .is_none_or(|at| now >= at && state.active == 0 && !state.probe_active)
    }

    pub fn try_acquire(self: &Arc<Self>) -> Option<ModelPermit> {
        self.acquire_at(Instant::now())
    }

    fn acquire_at(self: &Arc<Self>, now: Instant) -> Option<ModelPermit> {
        let mut state = self.0.lock().expect("admission lock");
        if !Self::ready(&state, now) {
            return None;
        }
        let probe = state.retry_at.is_some();
        state.active += 1;
        state.probe_active |= probe;
        state.next_start = now + Duration::from_millis(state.settings.start_interval_ms);
        Some(ModelPermit {
            gate: self.clone(),
            epoch: state.epoch,
            probe,
            tripped: Arc::new(AtomicBool::new(false)),
        })
    }
}

pub struct ModelPermit {
    gate: Arc<Admission>,
    epoch: u64,
    probe: bool,
    tripped: Arc<AtomicBool>,
}

impl ModelPermit {
    pub(crate) fn valid(&self) -> bool {
        self.gate.0.lock().expect("admission lock").epoch == self.epoch
    }
    pub(crate) fn belongs_to(&self, gate: &Arc<Admission>) -> bool {
        Arc::ptr_eq(&self.gate, gate)
    }
    pub(crate) fn observer(&self) -> ModelObserver {
        ModelObserver {
            gate: self.gate.clone(),
            tripped: self.tripped.clone(),
            epoch: self.epoch,
        }
    }
    pub(crate) fn finish(&self, success: bool) {
        let mut state = self.gate.0.lock().expect("admission lock");
        if success && self.probe && state.epoch == self.epoch {
            state.retry_at = None;
            state.strikes = 0;
        }
    }
}

impl Drop for ModelPermit {
    fn drop(&mut self) {
        let mut state = self.gate.0.lock().expect("admission lock");
        state.active -= 1;
        if self.probe {
            state.probe_active = false;
        }
    }
}

#[derive(Clone)]
pub struct ModelObserver {
    gate: Arc<Admission>,
    tripped: Arc<AtomicBool>,
    epoch: u64,
}

impl ModelObserver {
    pub(crate) fn valid(&self) -> bool {
        self.gate.0.lock().expect("admission lock").epoch == self.epoch
    }
    pub(crate) fn rate_limited(&self) {
        self.rate_limited_at(Instant::now());
    }
    pub(crate) fn quota_limited(&self) {
        self.limit_at(Instant::now(), true);
    }
    fn rate_limited_at(&self, now: Instant) {
        self.limit_at(now, false);
    }
    fn limit_at(&self, now: Instant, quota: bool) {
        if self.tripped.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut state = self.gate.0.lock().expect("admission lock");
        state.strikes = state.strikes.saturating_add(1);
        state.epoch = state.epoch.wrapping_add(1);
        let seconds = if quota {
            state.settings.maximum_backoff_seconds
        } else {
            state
                .settings
                .initial_backoff_seconds
                .saturating_mul(1u64 << state.strikes.saturating_sub(1).min(16))
                .min(state.settings.maximum_backoff_seconds)
        };
        state.retry_at = Some(
            state
                .retry_at
                .unwrap_or(now)
                .max(now + Duration::from_secs(seconds)),
        );
        tracing::warn!(
            backoff_seconds = seconds,
            cooldown_source = "application_policy",
            provider_reason = if quota {
                "usageLimitExceeded"
            } else {
                "unavailable"
            },
            retry_after = "unavailable",
            "model admission paused after native model limit"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shared_backoff_single_probe_and_old_success_cannot_reopen() {
        let gate = Arc::new(Admission::default());
        gate.configure(ModelAdmissionConfig {
            start_interval_ms: 0,
            ..Default::default()
        })
        .unwrap();
        let now = Instant::now();
        let first = gate.acquire_at(now).unwrap();
        let second = gate.acquire_at(now).unwrap();
        assert!(gate.acquire_at(now).is_none());
        first.observer().rate_limited_at(now);
        second.finish(true);
        drop(first);
        drop(second);
        assert!(gate.acquire_at(now + Duration::from_secs(29)).is_none());
        let probe = gate.acquire_at(now + Duration::from_secs(30)).unwrap();
        assert!(gate.acquire_at(now + Duration::from_secs(31)).is_none());
        probe
            .observer()
            .rate_limited_at(now + Duration::from_secs(31));
        probe.finish(true);
        drop(probe);
        assert!(gate.acquire_at(now + Duration::from_secs(90)).is_none());
        let probe = gate.acquire_at(now + Duration::from_secs(91)).unwrap();
        drop(probe); // cancellation releases the half-open lease, not the work
        let probe = gate.acquire_at(now + Duration::from_secs(92)).unwrap();
        probe.finish(true);
        drop(probe);
        assert!(gate.acquire_at(now + Duration::from_secs(92)).is_some());
    }
    #[test]
    fn native_structured_errors_do_not_confuse_quota_and_transient_rate_limit() {
        let f = ModelFailure::from_native(&serde_json::json!({"codexErrorInfo":{"httpConnectionFailed":{"httpStatusCode":429}},"message":"private"})).unwrap();
        assert_eq!(f.kind, "model_rate_limited");
        assert!(!f.to_string().contains("private"));
        assert_eq!(
            ModelFailure::from_native(&serde_json::json!({"codexErrorInfo":"usageLimitExceeded"}))
                .unwrap()
                .kind,
            "model_usage_limit"
        );
        assert!(
            ModelFailure::from_native(&serde_json::json!({"message":"document 429"})).is_none()
        );
    }
}

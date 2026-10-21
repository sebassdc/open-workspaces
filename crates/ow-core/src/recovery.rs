//! Scheduling metadata. A durable point's age starts at capture, never at upload.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::{SystemTime, UNIX_EPOCH};

pub const INTERVAL: u64 = 30 * 60;
pub const HIBERNATED_DELAY: u64 = 10 * 60;
pub const LOSS_TARGET: u64 = 60 * 60;

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct Point {
    pub snapshot: String,
    pub captured_at: u64,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct Recovery {
    #[serde(default)]
    pub store: String,
    #[serde(default)]
    pub sleep: Option<String>,
    #[serde(default)]
    pub sleep_observed_at: u64,
    pub pending: Option<Point>,
    pub durable: Option<Point>,
    pub uploaded_at: Option<u64>,
    pub attempted_at: u64,
    pub failures: u32,
    pub error: Option<String>,
}

impl Recovery {
    pub fn retry_at(&self) -> u64 {
        self.attempted_at.saturating_add(if self.failures == 0 {
            60
        } else {
            (30u64 << self.failures.min(4)).min(300)
        })
    }

    pub fn due(&self, state: &str, sleep: Option<(&str, u64)>, now: u64) -> bool {
        if self.attempted_at != 0 && now < self.retry_at() {
            return false;
        }
        if self.pending.is_some() {
            return true;
        }
        match state {
            "running" => self
                .durable
                .as_ref()
                .is_none_or(|p| now.saturating_sub(p.captured_at) >= INTERVAL),
            "hibernated" => sleep.is_some_and(|(name, at)| {
                now.saturating_sub(at) >= HIBERNATED_DELAY
                    && self
                        .durable
                        .as_ref()
                        .is_none_or(|p| p.source.as_deref() != Some(name))
            }),
            _ => false,
        }
    }

    pub fn summary(&self, now: u64) -> Value {
        let age = self
            .durable
            .as_ref()
            .map(|p| now.saturating_sub(p.captured_at));
        json!({
            "last_durable_at": self.durable.as_ref().map(|p| p.captured_at),
            "uploaded_at": self.uploaded_at,
            "age_seconds": age,
            "overdue": age.is_none_or(|age| age > LOSS_TARGET),
            "snapshot": self.durable.as_ref().map(|p| &p.snapshot),
            "pending": self.pending.as_ref().map(|p| &p.snapshot),
            "error": self.error,
            "failures": self.failures,
            "retry_at": if self.attempted_at == 0 { None } else { Some(self.retry_at()) },
            "retention": "latest local point plus pending; off-host points kept until explicit GC",
            "semantics": "disk recovery: a running box's point is its disk alone and restores as a cold boot; a hibernated box's point keeps RAM, which resumes only on a compatible runtime, CPU and kernel"
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scheduling_counts_upload_time_and_retries_the_same_point() {
        let mut r = Recovery::default();
        assert!(r.due("running", None, 1000));
        r.durable = Some(Point {
            snapshot: "r1".into(),
            captured_at: 1000,
            ..Point::default()
        });
        r.uploaded_at = Some(1600);
        assert!(!r.due("running", None, 2799));
        assert!(r.due("running", None, 2800));
        assert_eq!(r.summary(4601)["overdue"], true);
        r.pending = r.durable.clone();
        r.failures = 1;
        r.attempted_at = 3000;
        assert!(!r.due("stopped", None, 3059));
        assert!(r.due("stopped", None, 3060));
        let restored: Recovery = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(restored.pending.unwrap().snapshot, "r1");
    }
    #[test]
    fn hibernation_waits_ten_minutes_and_uploads_each_sleep_once() {
        let mut r = Recovery::default();
        assert!(!r.due("hibernated", Some(("sleep-1", 1000)), 1599));
        assert!(r.due("hibernated", Some(("sleep-1", 1000)), 1600));
        r.durable = Some(Point {
            source: Some("sleep-1".into()),
            ..Point::default()
        });
        assert!(!r.due("hibernated", Some(("sleep-1", 1000)), 9999));
        assert!(r.due("hibernated", Some(("sleep-2", 2000)), 9999));
        assert!(!r.due("stopped", None, 9999));
    }
}

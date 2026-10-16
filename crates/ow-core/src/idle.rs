//! Idle bookkeeping for automatic hibernation.
//!
//! A running machine is idle once, for the whole timeout, it has had no open
//! stream (SSH, terminal, tunnel), no operation, and almost no guest CPU or
//! network traffic. Background work that keeps the guest busy keeps it awake.
use anyhow::{Context, Result, ensure};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

/// Below these rates a sample counts as quiet: 2% of one CPU (Linux clock
/// ticks are 1/100 s) and 1 KiB/s of guest network traffic.
const CPU_TICKS_PER_SECOND: f64 = 2.0;
const NET_BYTES_PER_SECOND: f64 = 1024.0;

pub const ENV: &str = "OW_IDLE_HIBERNATE_SECS";

#[derive(Default)]
pub struct Idle {
    after: Option<Duration>,
    seen: BTreeMap<String, Seen>,
}

struct Seen {
    active: Instant,
    streams: usize,
    sample: Option<(Instant, u64, u64)>,
}

impl Idle {
    pub fn new(after: Option<Duration>) -> Self {
        Self {
            after,
            seen: BTreeMap::new(),
        }
    }

    /// `OW_IDLE_HIBERNATE_SECS`: unset or 0 turns automatic hibernation off.
    pub fn from_env() -> Result<Self> {
        let after = match std::env::var(ENV) {
            Err(std::env::VarError::NotPresent) => None,
            Ok(v) => {
                let secs: u64 = v.parse().with_context(|| format!("invalid {ENV}"))?;
                ensure!(secs <= 7 * 24 * 3600, "{ENV} is at most one week");
                (secs > 0).then(|| Duration::from_secs(secs))
            }
            Err(e) => return Err(e.into()),
        };
        Ok(Self::new(after))
    }

    pub fn enabled(&self) -> bool {
        self.after.is_some()
    }

    fn entry(&mut self, id: &str, now: Instant) -> &mut Seen {
        self.seen.entry(id.to_owned()).or_insert(Seen {
            active: now,
            streams: 0,
            sample: None,
        })
    }

    /// An operation or a started machine: the idle clock restarts.
    pub fn touch(&mut self, id: &str, now: Instant) {
        self.entry(id, now).active = now;
    }

    pub fn open(&mut self, id: &str, now: Instant) {
        let seen = self.entry(id, now);
        seen.streams += 1;
        seen.active = now;
    }

    pub fn close(&mut self, id: &str, now: Instant) {
        let seen = self.entry(id, now);
        seen.streams = seen.streams.saturating_sub(1);
        seen.active = now;
    }

    /// Records the machine's cumulative CPU ticks and network bytes; true once it
    /// has been idle for the whole timeout.
    pub fn due(&mut self, id: &str, cpu_ticks: u64, net_bytes: u64, now: Instant) -> bool {
        let Some(after) = self.after else {
            return false;
        };
        let seen = self.entry(id, now);
        if let Some((at, cpu, net)) = seen.sample {
            let secs = now.duration_since(at).as_secs_f64().max(0.001);
            let busy = cpu_ticks.saturating_sub(cpu) as f64 > CPU_TICKS_PER_SECOND * secs
                || net_bytes.saturating_sub(net) as f64 > NET_BYTES_PER_SECOND * secs;
            if busy {
                seen.active = now;
            }
        }
        seen.sample = Some((now, cpu_ticks, net_bytes));
        seen.streams == 0 && now.duration_since(seen.active) >= after
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_needs_no_streams_no_activity_and_a_quiet_guest() {
        let t0 = Instant::now();
        let s = |n| t0 + Duration::from_secs(n);
        let mut idle = Idle::new(Some(Duration::from_secs(60)));
        idle.touch("a", t0);
        assert!(!idle.due("a", 0, 0, s(30)));
        // A quiet guest: a few ticks and a little traffic over 30 s.
        assert!(idle.due("a", 10, 4096, s(60)));

        // An open stream keeps it awake however quiet it is.
        idle.open("a", s(60));
        assert!(!idle.due("a", 10, 4096, s(600)));
        idle.close("a", s(600));
        assert!(!idle.due("a", 10, 4096, s(630)));
        assert!(idle.due("a", 10, 4096, s(660)));

        // CPU work or network traffic restarts the clock.
        idle.touch("b", t0);
        assert!(!idle.due("b", 0, 0, s(50)));
        assert!(!idle.due("b", 1000, 0, s(60)), "busy CPU counted as idle");
        assert!(!idle.due("b", 1000, 0, s(100)));
        assert!(
            !idle.due("b", 1000, 10_000_000, s(120)),
            "traffic counted as idle"
        );
        assert!(idle.due("b", 1000, 10_000_000, s(180)));

        // Operations restart it too, and closing a stream never underflows.
        idle.touch("b", s(180));
        assert!(!idle.due("b", 1000, 10_000_000, s(200)));
        idle.close("c", t0);
        idle.open("c", t0);
        assert!(!idle.due("c", 0, 0, s(1000)));
    }

    #[test]
    fn disabled_never_hibernates() {
        let mut off = Idle::new(None);
        off.touch("a", Instant::now());
        assert!(!off.due("a", 0, 0, Instant::now() + Duration::from_secs(86400)));
        assert!(!off.enabled());
    }
}

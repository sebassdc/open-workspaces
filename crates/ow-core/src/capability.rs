//! Capability flags a compute provider advertises; the CLI checks them before each command.
use anyhow::{Result, ensure};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Capability {
    /// Named paired RAM+disk snapshots, forks and restores from them.
    Snapshot,
    /// Fork of a running machine without a prior named snapshot.
    LiveFork,
    Hibernate,
    /// Commands, terminals and file transfer in the guest.
    Exec,
    /// OpenSSH into the guest.
    Ssh,
}

impl Capability {
    pub const ALL: [Capability; 5] = [
        Self::Snapshot,
        Self::LiveFork,
        Self::Hibernate,
        Self::Exec,
        Self::Ssh,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Snapshot => "snapshot",
            Self::LiveFork => "live-fork",
            Self::Hibernate => "hibernate",
            Self::Exec => "exec",
            Self::Ssh => "ssh",
        }
    }

    /// The capability a worker operation such as `{"op":"fork"}` needs, if any.
    pub fn required(request: &Value) -> Option<Self> {
        Some(match request["op"].as_str()? {
            "snapshot" | "snapshots" | "snapshot-delete" | "restore" => Self::Snapshot,
            "fork" if request["snapshot"].is_null() => Self::LiveFork,
            "fork" => Self::Snapshot,
            "hibernate" => Self::Hibernate,
            "exec" | "shell-exec" | "terminal" | "put" | "get" => Self::Exec,
            "ssh" | "ssh-info" | "ssh-keys" => Self::Ssh,
            _ => return None,
        })
    }
}

/// A set of [`Capability`] flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Capabilities(u8);

impl Capabilities {
    pub const ALL: Self = Self::of(&Capability::ALL);

    pub const fn of(list: &[Capability]) -> Self {
        let mut bits = 0;
        let mut i = 0;
        while i < list.len() {
            bits |= 1 << list[i] as u8;
            i += 1;
        }
        Self(bits)
    }

    pub fn contains(self, capability: Capability) -> bool {
        self.0 & (1 << capability as u8) != 0
    }

    pub fn names(self) -> Vec<&'static str> {
        Capability::ALL
            .into_iter()
            .filter(|c| self.contains(*c))
            .map(Capability::name)
            .collect()
    }

    /// Refuses an operation whose capability `provider` lacks.
    pub fn check(self, provider: &str, request: &Value) -> Result<()> {
        if let Some(needed) = Capability::required(request) {
            ensure!(
                self.contains(needed),
                "{provider} does not support {} (supported: {})",
                needed.name(),
                self.names().join(", ")
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn operations_map_to_flags() {
        for (request, needed) in [
            (json!({"op":"snapshot"}), Some(Capability::Snapshot)),
            (json!({"op":"restore"}), Some(Capability::Snapshot)),
            (
                json!({"op":"fork","snapshot":null}),
                Some(Capability::LiveFork),
            ),
            (json!({"op":"fork"}), Some(Capability::LiveFork)),
            (
                json!({"op":"fork","snapshot":"s"}),
                Some(Capability::Snapshot),
            ),
            (json!({"op":"hibernate"}), Some(Capability::Hibernate)),
            (json!({"op":"terminal"}), Some(Capability::Exec)),
            (json!({"op":"get"}), Some(Capability::Exec)),
            (json!({"op":"ssh-keys"}), Some(Capability::Ssh)),
            (json!({"op":"create"}), None),
            (json!({"op":"status"}), None),
            (json!({}), None),
        ] {
            assert_eq!(Capability::required(&request), needed, "{request}");
        }
    }

    #[test]
    fn check_refuses_missing_flags() {
        let exec = Capabilities::of(&[Capability::Exec]);
        assert_eq!(exec.names(), ["exec"]);
        assert!(exec.check("mac", &json!({"op":"exec"})).is_ok());
        assert!(exec.check("mac", &json!({"op":"start"})).is_ok());
        let error = exec.check("mac", &json!({"op":"fork"})).unwrap_err();
        assert_eq!(
            error.to_string(),
            "mac does not support live-fork (supported: exec)"
        );
        assert_eq!(
            Capabilities::ALL.names(),
            ["snapshot", "live-fork", "hibernate", "exec", "ssh"]
        );
        assert!(
            Capabilities::default()
                .check("x", &json!({"op":"ssh"}))
                .is_err()
        );
    }
}

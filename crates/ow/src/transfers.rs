//! Dormant controller contracts. No transport, scheduler or worker activation lives here.
#![allow(dead_code)] // T3 wires these persisted contracts into orchestration.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Kind {
    Move,
    Wake,
    Park,
    Fork,
    Restore,
}
impl Kind {
    pub fn operation(self) -> &'static str {
        match self {
            Self::Move => "move",
            Self::Wake => "wake",
            Self::Park => "park",
            Self::Fork => "fork",
            Self::Restore => "restore",
        }
    }
    pub fn permission(self) -> &'static str {
        if self == Self::Wake {
            "start"
        } else {
            self.operation()
        }
    }
    pub fn needs_fence(self) -> bool {
        matches!(self, Self::Move | Self::Park | Self::Restore)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Step {
    Preparing,
    Exported,
    Importing,
    ImportUncertain,
    Prepared,
    Committed,
    StartUncertain,
    Started,
    Succeeded,
    Aborted,
    Failed,
}
impl Step {
    pub fn terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Aborted | Self::Failed)
    }
    pub fn before_commit(self) -> bool {
        matches!(
            self,
            Self::Preparing
                | Self::Exported
                | Self::Importing
                | Self::ImportUncertain
                | Self::Prepared
        )
    }
    pub fn permits(self, kind: Kind, next: Self) -> bool {
        match (self, next) {
            (Self::Preparing, Self::Exported) => kind.needs_fence(),
            (Self::Preparing, Self::Importing) => !kind.needs_fence(),
            (Self::Exported, Self::Prepared) => kind == Kind::Park,
            (Self::Exported, Self::Importing) => kind != Kind::Park,
            (Self::Importing, Self::ImportUncertain | Self::Prepared) => true,
            (Self::ImportUncertain, Self::Prepared) => true,
            (Self::Committed, Self::StartUncertain | Self::Started) => kind != Kind::Park,
            (Self::StartUncertain, Self::Started) => true,
            (Self::Committed, Self::Succeeded) => kind == Kind::Park,
            (Self::Started, Self::Succeeded) => true,
            (_, Self::Aborted | Self::Failed) => self.before_commit(),
            _ => false, // Commit is a separate conditional catalog transaction.
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Child {
    pub id: String,
    pub name: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Intent {
    pub kind: Kind,
    pub machine: String, // authorized logical address, never artifact-provided ownership
    pub target: Option<String>, // None means parked, not an inferred local runner
    pub capture: Option<String>, // existing F-store artifact ID
    pub child: Option<Child>,
    pub memory_mib: u32,
    pub cpus: u32,
    pub disk_bytes: u64,
    pub accept_cold: bool,
}
#[derive(Default)]
pub(crate) struct Evidence {
    pub capture: Option<String>,
    pub source_fenced: bool,
    pub target_prepared: bool,
    pub target_excluded: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Transfer {
    pub id: i64,
    pub operation: i64,
    pub kind: Kind,
    pub source: String,
    pub machine: String,
    pub source_node: String,
    pub source_epoch: i64,
    pub source_capture: Option<String>,
    pub target: Option<String>,
    pub epoch: i64,
    pub capture: Option<String>,
    pub step: Step,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PublicSshPolicy {
    Disabled,
    Enrolled(ow_core::ssh_guest::Policy),
}

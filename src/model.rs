use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// Constructed only after the Matrix transport has decrypted and authenticated an event.
/// Tests may construct fixtures. Do not deserialize this type from an untrusted HTTP API.
#[derive(Clone, Debug)]
pub struct Incoming {
    pub event_id: String,
    pub room_id: String,
    pub sender: String,
    pub body: String,
    pub thread_root: Option<String>,
    pub reply_to: Option<String>,
    pub mentions: BTreeSet<String>,
    pub encrypted: bool,
    pub verified_device: bool,
    /// SDK linked this message to a known sender device without a trust violation.
    /// This is account/device-list trust, not independently verified identity.
    pub known_sender_device: bool,
}

#[derive(Clone, Debug)]
pub struct RoomSnapshot {
    pub joined: bool,
    pub encrypted: bool,
    pub members: BTreeSet<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Conversation {
    pub key: String,
    pub room_id: String,
    pub thread_root: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub conversation: Conversation,
    pub prompt: String,
    pub status: RunStatus,
    pub session_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Queued,
    Running,
    WaitingApproval,
    Cancelling,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::WaitingApproval => "waiting_approval",
            Self::Cancelling => "cancelling",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        }
    }
    pub fn active(self) -> bool {
        matches!(
            self,
            Self::Queued | Self::Running | Self::WaitingApproval | Self::Cancelling
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Outbound {
    pub transaction_id: String,
    pub conversation: Conversation,
    pub body: String,
    #[serde(default)]
    pub reaction: Option<Reaction>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Reaction {
    pub event_id: String,
    pub key: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PermissionOption {
    pub id: String,
    pub label: String,
    /// Kept generic: allow_once, allow_always, reject_once or reject_always.
    pub kind: String,
}

#[derive(Clone, Debug)]
pub enum AgentEvent {
    SessionReady {
        run_id: String,
        session_id: String,
    },
    Text {
        run_id: String,
        text: String,
    },
    Tool {
        run_id: String,
        title: String,
    },
    Permission {
        run_id: String,
        request_id: String,
        title: String,
        options: Vec<PermissionOption>,
    },
    Finished {
        run_id: String,
        status: RunStatus,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    Start {
        run_id: String,
    },
    Cancel {
        run_id: String,
    },
    Decide {
        run_id: String,
        request_id: String,
        option_id: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Disposition {
    Ignored,
    Denied,
    Duplicate,
    Accepted,
}

#[derive(Clone, Debug)]
pub struct Handled {
    pub disposition: Disposition,
    pub effects: Vec<Effect>,
}

impl Handled {
    pub fn new(disposition: Disposition) -> Self {
        Self {
            disposition,
            effects: vec![],
        }
    }
}

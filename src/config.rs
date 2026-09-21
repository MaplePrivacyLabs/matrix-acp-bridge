use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub bot_user_id: String,
    pub homeserver: String,
    pub state_dir: PathBuf,
    pub harness: HarnessConfig,
    pub rooms: Vec<RoomPolicy>,
    #[serde(default = "default_approval_ttl")]
    pub approval_ttl_seconds: u64,
    #[serde(default = "default_concurrency")]
    pub max_concurrent_runs: usize,
    /// Optional shared socket path when bridge and agent have different OS users.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools_socket: Option<PathBuf>,
}

fn default_approval_ttl() -> u64 {
    300
}
fn default_concurrency() -> usize {
    2
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessConfig {
    pub program: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    pub workspace: PathBuf,
    /// Explicit child environment only. Never inherit the bridge environment.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Optional ACP mode. When set, the agent must advertise and apply it.
    /// Omit for agents that configure permissions outside ACP modes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Steering::is_after_turn")]
    pub steering: Steering,
}

/// Provider-specific delivery is explicit; ordinary ACP never receives concurrent prompts.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Steering {
    #[default]
    AfterTurn,
    /// Codex ACP accepts another prompt as input to its running turn.
    ConcurrentPrompt,
    /// Grok Build's native safe-point interjection, including late-turn drainage.
    GrokInterject,
}
impl Steering {
    fn is_after_turn(&self) -> bool {
        *self == Self::AfterTurn
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoomPolicy {
    pub room_id: String,
    pub operators: BTreeSet<String>,
    /// Account trust uses Matrix's authenticated device list; verified adds SAS.
    /// Legacy configs retain verified trust unless explicitly changed.
    #[serde(default, skip_serializing_if = "OperatorTrust::is_verified")]
    pub operator_trust: OperatorTrust,
    /// All permitted readers, including the bot. Unknown joined readers block work.
    #[serde(default)]
    pub audience: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "AudiencePolicy::is_configured")]
    pub audience_policy: AudiencePolicy,
    #[serde(default)]
    pub conversation: ConversationMode,
    /// Automatic decisions only select an ACP allow option offered by the agent.
    #[serde(default, skip_serializing_if = "ToolApproval::is_manual")]
    pub tool_approval: ToolApproval,
    /// Explicit mode publishes only calls to send_message_to_thread.
    #[serde(default, skip_serializing_if = "MessageDelivery::is_automatic")]
    pub message_delivery: MessageDelivery,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MessageDelivery {
    #[default]
    Automatic,
    Explicit,
}
impl MessageDelivery {
    fn is_automatic(&self) -> bool {
        *self == Self::Automatic
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Automatic => "automatic",
            Self::Explicit => "explicit",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConversationMode {
    Room,
    #[default]
    Thread,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolApproval {
    #[default]
    Manual,
    Automatic,
}

impl ToolApproval {
    fn is_manual(&self) -> bool {
        *self == Self::Manual
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OperatorTrust {
    Account,
    #[default]
    Verified,
}

impl OperatorTrust {
    fn is_verified(&self) -> bool {
        *self == Self::Verified
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AudiencePolicy {
    #[default]
    Configured,
    RoomMembership,
}

impl AudiencePolicy {
    fn is_configured(&self) -> bool {
        *self == Self::Configured
    }
}

impl RoomPolicy {
    pub fn permits_members(&self, members: &BTreeSet<String>) -> bool {
        self.audience_policy == AudiencePolicy::RoomMembership || members.is_subset(&self.audience)
    }
    pub fn permits_context_sender(&self, sender: &str) -> bool {
        self.audience_policy == AudiencePolicy::RoomMembership || self.audience.contains(sender)
    }
    pub fn audience_binding(&self, members: &BTreeSet<String>) -> String {
        if self.audience_policy == AudiencePolicy::RoomMembership {
            "room-membership".into()
        } else {
            digest(&serde_json::to_vec(members).expect("members serialize"))
        }
    }
}

impl Config {
    pub fn parse(text: &str) -> Result<Self> {
        let config: Self = toml::from_str(text).context("invalid bridge configuration")?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        self.bot_user_id
            .parse::<ruma::OwnedUserId>()
            .context("invalid bot Matrix ID")?;
        let url = url::Url::parse(&self.homeserver).context("invalid homeserver URL")?;
        ensure!(
            url.scheme() == "https" && url.host_str().is_some(),
            "homeserver must use HTTPS"
        );
        ensure!(
            url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "homeserver URL must not contain credentials, query or fragment"
        );
        ensure!(self.state_dir.is_absolute(), "state_dir must be absolute");
        ensure!(
            self.harness.program.is_absolute() && self.harness.workspace.is_absolute(),
            "harness program and workspace must be absolute"
        );
        ensure!(
            self.harness
                .mode
                .as_ref()
                .is_none_or(|mode| !mode.trim().is_empty()),
            "omit harness mode instead of providing an empty value"
        );
        ensure!(
            !self.rooms.is_empty(),
            "at least one room policy is required"
        );
        ensure!(
            (1..=3600).contains(&self.approval_ttl_seconds),
            "approval TTL must be 1 to 3600 seconds"
        );
        ensure!(
            self.max_concurrent_runs <= 64,
            "worker concurrency must be 0 (unlimited) to 64"
        );
        ensure!(
            self.tools_socket.as_ref().is_none_or(|p| p.is_absolute()),
            "tools socket must be absolute"
        );
        let mut rooms = BTreeSet::new();
        for room in &self.rooms {
            room.room_id
                .parse::<ruma::OwnedRoomId>()
                .context("invalid room ID")?;
            ensure!(rooms.insert(&room.room_id), "duplicate room policy");
            ensure!(!room.operators.is_empty(), "room must have an operator");
            ensure!(
                !room.operators.contains(&self.bot_user_id),
                "bot cannot be its own operator"
            );
            ensure!(
                room.audience_policy == AudiencePolicy::RoomMembership
                    || room.audience.contains(&self.bot_user_id),
                "audience must include bot"
            );
            ensure!(
                room.audience_policy == AudiencePolicy::RoomMembership
                    || room.operators.is_subset(&room.audience),
                "operators must belong to configured audience"
            );
            for user in &room.audience {
                user.parse::<ruma::OwnedUserId>()
                    .context("invalid audience ID")?;
            }
        }
        for key in self.harness.env.keys() {
            ensure!(
                !key.is_empty() && !key.contains(['=', '\0']),
                "invalid child environment key"
            );
        }
        Ok(())
    }

    pub fn tools_socket_path(&self) -> PathBuf {
        self.tools_socket
            .clone()
            .unwrap_or_else(|| self.state_dir.join("tools.sock"))
    }

    /// Binding identity includes credential configuration, but never stores its plaintext.
    pub fn fingerprint(&self) -> String {
        digest(&serde_json::to_vec(self).expect("config serialization is infallible"))
    }

    /// In room-membership mode, access rules apply on each request; adding an
    /// operator or reader does not invalidate a coding session. Runtime/credential
    /// changes still get a new binding. Legacy configured audiences remain strict.
    pub fn binding_fingerprint(&self, room_id: &str) -> String {
        let Some(room) = self.room(room_id) else {
            return self.fingerprint();
        };
        if room.audience_policy == AudiencePolicy::Configured {
            return self.fingerprint();
        }
        digest(
            &serde_json::to_vec(&(
                &self.bot_user_id,
                &self.homeserver,
                &self.harness,
                &room.room_id,
                room.conversation,
                "room-membership-v1",
            ))
            .expect("binding serializes"),
        )
    }

    pub fn room(&self, id: &str) -> Option<&RoomPolicy> {
        self.rooms.iter().find(|room| room.room_id == id)
    }
}

pub(crate) fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

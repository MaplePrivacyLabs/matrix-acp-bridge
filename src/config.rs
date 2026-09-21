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
    /// Required mode. A backend that cannot apply it is rejected.
    pub mode: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoomPolicy {
    pub room_id: String,
    pub operators: BTreeSet<String>,
    /// All permitted readers, including the bot. Unknown joined readers block work.
    pub audience: BTreeSet<String>,
    #[serde(default)]
    pub conversation: ConversationMode,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConversationMode {
    Room,
    #[default]
    Thread,
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
            !self.harness.mode.trim().is_empty(),
            "an explicit harness mode is required"
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
            (1..=64).contains(&self.max_concurrent_runs),
            "worker concurrency must be 1 to 64"
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
                room.audience.contains(&self.bot_user_id),
                "audience must include bot"
            );
            ensure!(
                room.operators.is_subset(&room.audience),
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

    /// Binding identity includes credential configuration, but never stores its plaintext.
    pub fn fingerprint(&self) -> String {
        digest(&serde_json::to_vec(self).expect("config serialization is infallible"))
    }

    pub fn room(&self, id: &str) -> Option<&RoomPolicy> {
        self.rooms.iter().find(|room| room.room_id == id)
    }
}

pub(crate) fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

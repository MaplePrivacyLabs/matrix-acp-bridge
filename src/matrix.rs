//! Matrix SDK adapter. Uses the enrolled Client supplied by the live launcher.
//! This module does not obtain credentials.

use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use anyhow::{Result, ensure};
use matrix_sdk::{
    Client, Room, RoomMemberships, RoomState,
    config::{SyncSettings, SyncToken},
    deserialized_responses::{
        TimelineEvent, TimelineEventKind, VerificationLevel, VerificationState,
    },
    room::{IncludeRelations, MessagesOptions, RelationsOptions},
};
use matrix_sdk_crypto::CollectStrategy;
use ruma::events::{
    AnySyncMessageLikeEvent, AnySyncTimelineEvent, SyncMessageLikeEvent,
    relation::{RelationType, Thread},
    room::message::{MessageType, Relation, RoomMessageEventContent},
};
use ruma::{api::client::sync::sync_events, serde::Raw};
use serde::{Deserialize, Serialize};

use crate::{
    config::Config,
    model::{Attachment, Incoming, Outbound, RoomSnapshot},
    runner::Runner,
    store::Store,
};

/// Room membership and the configured audience govern who can read replies.
/// Verification is still required separately when admitting agent commands.
/// The SDK continues to exclude devices explicitly marked as blocked.
pub const REPLY_KEY_RECIPIENT_STRATEGY: CollectStrategy = CollectStrategy::AllDevices;

#[derive(Clone)]
pub struct MatrixAdapter {
    client: Client,
    allowed_rooms: BTreeSet<String>,
}

pub struct Batch {
    pub next_token: String,
    pub snapshots: BTreeMap<String, RoomSnapshot>,
    pub events: Vec<(Incoming, RoomSnapshot)>,
    pub anchors: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize)]
struct WireBatch {
    since: Option<String>,
    rooms: BTreeMap<String, WireRoom>,
}

#[derive(Serialize, Deserialize)]
struct WireRoom {
    limited: bool,
    prev_batch: Option<String>,
    events: Vec<Raw<AnySyncTimelineEvent>>,
}

impl MatrixAdapter {
    pub fn from_client(config: &Config, client: Client) -> Result<Self> {
        ensure!(
            client
                .user_id()
                .is_some_and(|id| id.as_str() == config.bot_user_id),
            "Matrix client identity does not match configured bot"
        );
        Ok(Self {
            client,
            allowed_rooms: config.rooms.iter().map(|r| r.room_id.clone()).collect(),
        })
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Journal raw encrypted timelines before letting the SDK advance its cursor.
    /// A second, non-waiting SDK sync from the same cursor handles keys and state.
    /// Decode the journal copy: the SDK may intentionally suppress a repeated batch.
    /// First sync establishes a baseline without executing historical messages.
    pub async fn poll(&self, store: &mut Store) -> Result<Batch> {
        let (next_token, wire) = if let Some((token, payload)) = store.staged_sync()? {
            (token, serde_json::from_str::<WireBatch>(&payload)?)
        } else {
            let since = store.sync_token()?;
            let mut request = sync_events::v3::Request::new();
            request.since = since.clone();
            request.timeout = Some(Duration::from_secs(2));
            request.use_state_after = true;
            let response = self.client.send(request).await?;
            let wire = WireBatch {
                since,
                rooms: response
                    .rooms
                    .join
                    .into_iter()
                    .filter(|(id, _)| self.allowed_rooms.contains(id.as_str()))
                    .map(|(id, room)| {
                        (
                            id.to_string(),
                            WireRoom {
                                limited: room.timeline.limited,
                                prev_batch: room.timeline.prev_batch,
                                events: room.timeline.events,
                            },
                        )
                    })
                    .collect(),
            };
            store.stage_sync(&response.next_batch, &serde_json::to_string(&wire)?)?;
            (response.next_batch, wire)
        };
        let initial = wire.since.is_none();
        let token = wire
            .since
            .map(SyncToken::Specific)
            .unwrap_or(SyncToken::NoToken);
        self.client
            .sync_once(SyncSettings::default().token(token).timeout(Duration::ZERO))
            .await?;
        let mut events = vec![];
        let mut snapshots = BTreeMap::new();
        let mut anchors = BTreeMap::new();
        for id in &self.allowed_rooms {
            let parsed: ruma::OwnedRoomId = id.parse()?;
            let snapshot = match self.client.get_room(&parsed) {
                Some(room) => {
                    if room.state() == RoomState::Joined {
                        room.sync_members().await?;
                    }
                    room_snapshot(&room).await?
                }
                None => RoomSnapshot {
                    joined: false,
                    encrypted: false,
                    members: BTreeSet::new(),
                },
            };
            snapshots.insert(id.clone(), snapshot);
        }
        for (room_id, update) in wire.rooms {
            if !self.allowed_rooms.contains(room_id.as_str()) {
                continue;
            }
            if let Some(last) = update.events.last()
                && let Some(event_id) = last.get_field::<String>("event_id")?
            {
                anchors.insert(room_id.clone(), event_id);
            }
            if initial {
                continue; // Establish a cursor without running historical messages.
            }
            let snapshot = snapshots
                .get(room_id.as_str())
                .expect("configured snapshot");
            let id: ruma::OwnedRoomId = room_id.parse()?;
            let room = self
                .client
                .get_room(&id)
                .ok_or_else(|| anyhow::anyhow!("synced room missing"))?;
            let anchor_in_window = if let Some(anchor) = store.room_anchor(&room_id)? {
                update.events.iter().any(|raw| {
                    raw.get_field::<String>("event_id")
                        .ok()
                        .flatten()
                        .as_deref()
                        == Some(anchor.as_str())
                })
            } else {
                false
            };
            if update.limited && !anchor_in_window {
                let anchor = store.room_anchor(&room_id)?.ok_or_else(|| {
                    anyhow::anyhow!(
                        "Matrix timeline gap has no prior room anchor; retain staged cursor"
                    )
                })?;
                let start = update.prev_batch.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "Matrix timeline gap has no pagination token; retain staged cursor"
                    )
                })?;
                for event in backfill_to_anchor(&room, start, &anchor).await? {
                    if let Some(incoming) = decode(room_id.as_str(), &event)? {
                        events.push((incoming, snapshot.clone()));
                    }
                }
            }
            for raw in update.events {
                if raw.get_field::<String>("type")?.as_deref() != Some("m.room.encrypted") {
                    continue;
                }
                if !matches!(
                    raw.deserialize()?,
                    AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomEncrypted(
                        SyncMessageLikeEvent::Original(_)
                    ))
                ) {
                    continue;
                }
                let encrypted = Raw::from_json(raw.into_json());
                let event = room.decrypt_event(&encrypted, None).await?;
                ensure!(
                    !matches!(event.kind, TimelineEventKind::UnableToDecrypt { .. }),
                    "room has an undecryptable event; retain the cursor for key recovery"
                );
                if let Some(incoming) = decode(room_id.as_str(), &event)? {
                    events.push((incoming, snapshot.clone()));
                }
            }
        }
        Ok(Batch {
            next_token,
            snapshots,
            events,
            anchors,
        })
    }

    pub async fn snapshot(&self, room_id: &str) -> Result<RoomSnapshot> {
        ensure!(
            self.allowed_rooms.contains(room_id),
            "room is not configured"
        );
        let id: ruma::OwnedRoomId = room_id.parse()?;
        let room = self
            .client
            .get_room(&id)
            .ok_or_else(|| anyhow::anyhow!("room is not joined"))?;
        if room.state() == RoomState::Joined {
            room.sync_members().await?;
        }
        room_snapshot(&room).await
    }

    /// Called only by deliver(), which re-checks policy and audience before each send.
    /// Reusing transaction_id allows an uncertain HTTP send to be retried safely.
    async fn send(&self, message: &Outbound) -> Result<String> {
        ensure!(
            self.allowed_rooms.contains(&message.conversation.room_id),
            "room is not configured"
        );
        let id: ruma::OwnedRoomId = message.conversation.room_id.parse()?;
        let room = self
            .client
            .get_room(&id)
            .ok_or_else(|| anyhow::anyhow!("room is not joined"))?;
        ensure!(
            room.state() == RoomState::Joined && room.encryption_state().is_encrypted(),
            "refusing plaintext or unjoined delivery"
        );
        let txn: ruma::OwnedTransactionId = message.transaction_id.clone().into();
        let response = if let Some(reaction) = &message.reaction {
            let content = ruma::events::reaction::ReactionEventContent::new(
                ruma::events::relation::Annotation::new(
                    reaction.event_id.parse()?,
                    reaction.key.clone(),
                ),
            );
            room.send(content).with_transaction_id(txn).await?
        } else {
            room.send(render(message)?).with_transaction_id(txn).await?
        };
        Ok(response.response.event_id.to_string())
    }

    pub async fn ingest_batch(runner: &mut Runner, batch: Batch, now: i64) -> Result<()> {
        for (id, snapshot) in &batch.snapshots {
            runner.reconcile_room(id, snapshot, now).await?;
        }
        for (event, snapshot) in &batch.events {
            runner.ingest(event, snapshot, now).await?;
        }
        runner
            .bridge
            .store
            .checkpoint_sync_with_anchors(&batch.next_token, &batch.anchors)?;
        Ok(())
    }

    /// All decrypted text in the thread up to this request, including a parent
    /// written by a reader. Relations are paginated: there is no hidden thread cap.
    pub async fn context_for(&self, event: &Incoming) -> Result<Vec<Incoming>> {
        ensure!(
            self.allowed_rooms.contains(&event.room_id),
            "room is not configured"
        );
        let id: ruma::OwnedRoomId = event.room_id.parse()?;
        let room = self
            .client
            .get_room(&id)
            .ok_or_else(|| anyhow::anyhow!("room missing"))?;
        if let Some(root) = &event.thread_root {
            let root_id: ruma::OwnedEventId = root.parse()?;
            let parent = room.event(&root_id, None).await?;
            let mut history = vec![];
            append_context(&event.room_id, &parent, &mut history)?;
            let mut from = None;
            let mut tokens = BTreeSet::new();
            loop {
                let options = RelationsOptions {
                    from,
                    dir: ruma::api::Direction::Forward,
                    limit: Some(ruma::uint!(100)),
                    include_relations: IncludeRelations::RelationsOfType(RelationType::Thread),
                    ..Default::default()
                };
                let page = room.relations(root_id.clone(), options).await?;
                for item in page.chunk {
                    if item.raw().get_field::<String>("event_id")?.as_deref()
                        == Some(&event.event_id)
                    {
                        return Ok(history);
                    }
                    append_context(&event.room_id, &item, &mut history)?;
                }
                let next = page.next_batch_token.ok_or_else(|| {
                    anyhow::anyhow!("thread history did not reach the triggering event")
                })?;
                ensure!(
                    tokens.insert(next.clone()),
                    "thread history pagination repeated a token"
                );
                from = Some(next);
            }
        } else {
            // New room-level mentions get the nearby conversation. Existing
            // threads use the complete thread path above, never unrelated threads.
            let id: ruma::OwnedEventId = event.event_id.parse()?;
            let page = room
                .event_with_context(&id, false, ruma::uint!(40), None)
                .await?;
            let mut history = vec![];
            for item in page.events_before.into_iter().rev() {
                append_context(&event.room_id, &item, &mut history)?;
            }
            if let Some(reply) = &event.reply_to
                && !history.iter().any(|e| &e.event_id == reply)
            {
                let reply_id: ruma::OwnedEventId = reply.parse()?;
                let target = room.event(&reply_id, None).await?;
                append_context(&event.room_id, &target, &mut history)?;
            }
            Ok(history)
        }
    }

    pub async fn ingest_live_batch(
        &self,
        runner: &mut Runner,
        batch: Batch,
        now: i64,
    ) -> Result<()> {
        for (id, snapshot) in &batch.snapshots {
            runner.reconcile_room(id, snapshot, now).await?;
        }
        for (event, snapshot) in &batch.events {
            let context = if runner.bridge.needs_context(event, snapshot)? {
                Some(self.context_for(event).await?)
            } else {
                None
            };
            runner
                .ingest_with_context(event, snapshot, context.as_deref(), now)
                .await?;
        }
        runner
            .bridge
            .store
            .checkpoint_sync_with_anchors(&batch.next_token, &batch.anchors)?;
        Ok(())
    }

    pub async fn deliver(&self, runner: &mut Runner) -> Result<usize> {
        let mut count = 0;
        for message in runner.bridge.store.pending()? {
            let snapshot = self.snapshot(&message.conversation.room_id).await?;
            if !runner.bridge.may_deliver(&message, &snapshot)? {
                runner.block_message(&message.transaction_id)?;
                continue;
            }
            let event_id = self.send(&message).await?;
            runner
                .bridge
                .store
                .delivered(&message.transaction_id, &event_id)?;
            count += 1;
        }
        Ok(count)
    }
}

fn append_context(room: &str, event: &TimelineEvent, history: &mut Vec<Incoming>) -> Result<()> {
    ensure!(
        !matches!(event.kind, TimelineEventKind::UnableToDecrypt { .. }),
        "conversation context contains an undecryptable event; retain the request for key recovery"
    );
    if let Some(message) = decode(room, event)? {
        history.push(message);
    }
    Ok(())
}

/// Recover only a provably continuous gap. A missing anchor or a large history
/// holds the staged cursor for manual review instead of dropping requests.
async fn backfill_to_anchor(room: &Room, start: &str, anchor: &str) -> Result<Vec<TimelineEvent>> {
    let mut token = start.to_owned();
    let mut reversed = Vec::new();
    for _ in 0..10 {
        let mut options = MessagesOptions::backward();
        options.from = Some(token.clone());
        options.limit = ruma::uint!(50);
        let page = room.messages(options).await?;
        ensure!(
            !page.chunk.is_empty(),
            "Matrix timeline gap reached an empty page before its anchor"
        );
        for event in page.chunk {
            let event_id = event.raw().get_field::<String>("event_id")?;
            if event_id.as_deref() == Some(anchor) {
                reversed.reverse();
                return Ok(reversed);
            }
            ensure!(
                !matches!(event.kind, TimelineEventKind::UnableToDecrypt { .. }),
                "backfilled room has an undecryptable event; retain staged cursor"
            );
            reversed.push(event);
        }
        token = page.end.ok_or_else(|| {
            anyhow::anyhow!("Matrix timeline gap reached history end before its anchor")
        })?;
    }
    anyhow::bail!("Matrix timeline gap exceeds 500 events; retain staged cursor")
}

pub async fn build_client(config: &Config, store_passphrase: &str) -> Result<Client> {
    config.validate()?;
    ensure!(
        !store_passphrase.is_empty(),
        "Matrix crypto-store passphrase is required"
    );
    Ok(Client::builder()
        .homeserver_url(&config.homeserver)
        .sqlite_store(config.state_dir.join("matrix"), Some(store_passphrase))
        .handle_refresh_tokens()
        .with_room_key_recipient_strategy(REPLY_KEY_RECIPIENT_STRATEGY)
        .build()
        .await?)
}

pub async fn room_snapshot(room: &Room) -> Result<RoomSnapshot> {
    if room.state() != RoomState::Joined {
        return Ok(RoomSnapshot {
            joined: false,
            encrypted: room.encryption_state().is_encrypted(),
            members: BTreeSet::new(),
        });
    }
    Ok(RoomSnapshot {
        joined: room.state() == RoomState::Joined,
        encrypted: room.encryption_state().is_encrypted(),
        members: room
            .members(RoomMemberships::JOIN)
            .await?
            .into_iter()
            .map(|m| m.user_id().to_string())
            .collect(),
    })
}

/// Explicit account trust never accepts an unknown or contradictory key origin.
pub fn account_device_is_known(state: &VerificationState) -> bool {
    matches!(
        state,
        VerificationState::Verified
            | VerificationState::Unverified(
                VerificationLevel::UnverifiedIdentity | VerificationLevel::UnsignedDevice
            )
    )
}

pub fn decode(room_id: &str, event: &TimelineEvent) -> Result<Option<Incoming>> {
    let Some(info) = event.encryption_info() else {
        return Ok(None);
    };
    let AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
        SyncMessageLikeEvent::Original(message),
    )) = event.raw().deserialize()?
    else {
        return Ok(None);
    };
    let (body, attachment) = match message.content.msgtype {
        MessageType::Text(text) => (text.body, None),
        MessageType::Image(file) => (
            file.body.clone(),
            Some(Attachment {
                name: file.body,
                mime_type: file
                    .info
                    .and_then(|i| i.mimetype)
                    .unwrap_or_else(|| "image/jpeg".into()),
                source: serde_json::to_value(file.source)?,
            }),
        ),
        MessageType::File(file) => (
            file.body.clone(),
            Some(Attachment {
                name: file.filename.unwrap_or(file.body),
                mime_type: file
                    .info
                    .and_then(|i| i.mimetype)
                    .unwrap_or_else(|| "application/octet-stream".into()),
                source: serde_json::to_value(file.source)?,
            }),
        ),
        MessageType::Audio(file) => (
            file.body.clone(),
            Some(Attachment {
                name: file.body,
                mime_type: file
                    .info
                    .and_then(|i| i.mimetype)
                    .unwrap_or_else(|| "audio/ogg".into()),
                source: serde_json::to_value(file.source)?,
            }),
        ),
        MessageType::Video(file) => (
            file.body.clone(),
            Some(Attachment {
                name: file.body,
                mime_type: file
                    .info
                    .and_then(|i| i.mimetype)
                    .unwrap_or_else(|| "video/mp4".into()),
                source: serde_json::to_value(file.source)?,
            }),
        ),
        _ => return Ok(None),
    };
    let (thread_root, reply_to) = match message.content.relates_to {
        Some(Relation::Replacement(_)) => return Ok(None),
        Some(Relation::Thread(thread)) => (
            Some(thread.event_id.to_string()),
            thread.in_reply_to.map(|r| r.event_id.to_string()),
        ),
        Some(Relation::Reply(reply)) => (None, Some(reply.in_reply_to.event_id.to_string())),
        None => (None, None),
        _ => return Ok(None),
    };
    Ok(Some(Incoming {
        event_id: message.event_id.to_string(),
        room_id: room_id.into(),
        sender: message.sender.to_string(),
        body,
        attachment,
        thread_root,
        reply_to,
        mentions: message
            .content
            .mentions
            .map(|m| m.user_ids.into_iter().map(|id| id.to_string()).collect())
            .unwrap_or_default(),
        encrypted: true,
        known_sender_device: account_device_is_known(&info.verification_state)
            && info.sender == message.sender
            && info.forwarder.is_none(),
        verified_device: info.verification_state == VerificationState::Verified
            && info.sender == message.sender,
    }))
}

pub fn render(message: &Outbound) -> Result<RoomMessageEventContent> {
    let mut content = RoomMessageEventContent::text_plain(&message.body);
    if let Some(root) = &message.conversation.thread_root {
        let root: ruma::OwnedEventId = root.parse()?;
        content.relates_to = Some(Relation::Thread(Thread::plain(root.clone(), root)));
    }
    Ok(content)
}

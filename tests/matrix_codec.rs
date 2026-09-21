#![cfg(feature = "matrix")]
mod common;
use common::*;
use matrix_acp_bridge::{
    matrix::{Batch, MatrixAdapter, decode, render},
    model::*,
};
use matrix_sdk::deserialized_responses::TimelineEvent;
use matrix_sdk_crypto::{DecryptionSettings, EncryptionSettings, OlmMachine, TrustRequirement};
use ruma::{device_id, events::AnyMessageLikeEventContent, room_id, serde::Raw, user_id};
use serde_json::json;

#[tokio::test]
async fn sdk_megolm_roundtrip_retains_thread_and_plaintext_is_never_admitted() {
    // Entirely in-memory crypto. No Matrix client, server, token, or HTTP request.
    let user = user_id!("@owner:example.invalid");
    let room = room_id!("!engineering:example.invalid");
    let machine = OlmMachine::new(user, device_id!("OFFLINE_FIXTURE")).await;
    machine
        .share_room_key(
            room,
            std::iter::empty::<&ruma::UserId>(),
            EncryptionSettings::default(),
        )
        .await
        .unwrap();
    let output = Outbound {
        reaction: None,
        transaction_id: "fixed-txn".into(),
        conversation: Conversation {
            key: "fixture".into(),
            room_id: room.to_string(),
            thread_root: Some("$root".into()),
        },
        body: "secret fixture content".into(),
    };
    let content = render(&output).unwrap();
    let encrypted = machine
        .encrypt_room_event(
            room,
            AnyMessageLikeEventContent::RoomMessage(content.clone()),
        )
        .await
        .unwrap();
    let raw=Raw::from_json_string(json!({"event_id":"$encrypted","origin_server_ts":1,"sender":user,"type":"m.room.encrypted","content":encrypted.content}).to_string()).unwrap();
    assert!(!raw.json().get().contains("secret fixture content"));
    let decrypted = machine
        .decrypt_room_event(
            &raw,
            room,
            &DecryptionSettings {
                sender_device_trust_requirement: TrustRequirement::Untrusted,
            },
        )
        .await
        .unwrap();
    let timeline = TimelineEvent::from_decrypted(decrypted, None);
    let incoming = decode(room.as_str(), &timeline).unwrap().unwrap();
    assert_eq!(incoming.body, output.body);
    assert_eq!(incoming.thread_root.as_deref(), Some("$root"));
    assert!(incoming.encrypted);
    let plain=TimelineEvent::from_plaintext(Raw::from_json_string(json!({"event_id":"$plain","origin_server_ts":1,"sender":user,"type":"m.room.message","content":content}).to_string()).unwrap());
    assert!(decode(room.as_str(), &plain).unwrap().is_none());
    assert!(
        machine
            .decrypt_room_event(
                &raw,
                room_id!("!wrong:example.invalid"),
                &DecryptionSettings {
                    sender_device_trust_requirement: TrustRequirement::Untrusted
                }
            )
            .await
            .is_err()
    );
}

#[test]
fn rendered_matrix_thread_is_standard_and_transaction_identity_stays_external() {
    let output = Outbound {
        reaction: None,
        transaction_id: "stable-on-retry".into(),
        conversation: Conversation {
            key: "fixture".into(),
            room_id: "!engineering:example.invalid".into(),
            thread_root: Some("$root".into()),
        },
        body: "hello".into(),
    };
    let json = serde_json::to_value(render(&output).unwrap()).unwrap();
    assert_eq!(json["m.relates_to"]["rel_type"], "m.thread");
    assert_eq!(json["m.relates_to"]["event_id"], "$root");
    assert_eq!(json["body"], "hello");
    assert!(json.get("transaction_id").is_none());
}

#[tokio::test]
async fn batch_checkpoint_follows_durable_admission_and_replay_is_deduplicated() {
    let (mut runner, agent) = fixture(matrix_acp_bridge::offline::Behavior::Reply);
    let batch = || Batch {
        next_token: "fixture-token".into(),
        snapshots: std::collections::BTreeMap::from([(config().rooms[0].room_id.clone(), room())]),
        events: vec![(message("one", "once", None), room())],
        anchors: std::collections::BTreeMap::new(),
    };
    MatrixAdapter::ingest_batch(&mut runner, batch(), 100)
        .await
        .unwrap();
    finish(&mut runner).await;
    assert_eq!(
        runner.bridge.store.sync_token().unwrap().as_deref(),
        Some("fixture-token")
    );
    MatrixAdapter::ingest_batch(&mut runner, batch(), 103)
        .await
        .unwrap();
    assert_eq!(agent.observations.lock().unwrap().prompts.len(), 1);
}

#[tokio::test]
async fn replies_reach_unverified_readers_but_blocked_devices_stay_excluded() {
    use matrix_acp_bridge::matrix::REPLY_KEY_RECIPIENT_STRATEGY;
    use matrix_sdk_crypto::types::requests::AnyOutgoingRequest;
    use matrix_sdk_crypto::{CollectStrategy, EncryptionSyncChanges, LocalTrust};
    use ruma::{
        TransactionId,
        api::client::{
            keys::{claim_keys, get_keys},
            to_device::send_event_to_device,
        },
    };

    let bot = OlmMachine::new(user_id!("@bot:example.invalid"), device_id!("BOT")).await;
    let reader = OlmMachine::new(
        user_id!("@reader:example.invalid"),
        device_id!("UNVERIFIED"),
    )
    .await;
    let room = room_id!("!team:example.invalid");
    let upload = reader
        .outgoing_requests()
        .await
        .unwrap()
        .into_iter()
        .find_map(|r| {
            if let AnyOutgoingRequest::KeysUpload(keys) = r.request() {
                Some(keys.clone())
            } else {
                None
            }
        })
        .unwrap();
    let mut query = get_keys::v3::Response::new();
    query
        .device_keys
        .entry(reader.user_id().to_owned())
        .or_default()
        .insert(reader.device_id().to_owned(), upload.device_keys.unwrap());
    bot.mark_request_as_sent(&TransactionId::new(), &query)
        .await
        .unwrap();
    let device = bot
        .get_device(reader.user_id(), reader.device_id(), None)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !device.is_verified(),
        "test must exercise an unverified reader"
    );
    let (claim_id, _) = bot
        .get_missing_sessions(std::iter::once(reader.user_id()))
        .await
        .unwrap()
        .unwrap();
    let mut claim = claim_keys::v3::Response::new(Default::default());
    claim
        .one_time_keys
        .entry(reader.user_id().to_owned())
        .or_default()
        .insert(
            reader.device_id().to_owned(),
            upload.one_time_keys.into_iter().take(1).collect(),
        );
    bot.mark_request_as_sent(&claim_id, &claim).await.unwrap();

    // Reproduce the original refusal before applying the bridge's current policy.
    let withheld = bot
        .share_room_key(
            room,
            std::iter::once(reader.user_id()),
            EncryptionSettings {
                sharing_strategy: CollectStrategy::OnlyTrustedDevices,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(
        withheld
            .iter()
            .any(|r| r.event_type.to_string() == "m.room_key.withheld")
    );
    for request in &withheld {
        bot.mark_request_as_sent(&request.txn_id, &send_event_to_device::v3::Response::new())
            .await
            .unwrap();
    }

    let shared = bot
        .share_room_key(
            room,
            std::iter::once(reader.user_id()),
            EncryptionSettings {
                sharing_strategy: REPLY_KEY_RECIPIENT_STRATEGY,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let mut events = Vec::new();
    for request in &shared {
        for (user, devices) in &request.messages {
            assert_eq!(user, reader.user_id());
            for content in devices.values() {
                events.push(
                    Raw::from_json_string(
                        json!({
                            "sender":bot.user_id(), "type":request.event_type, "content":content
                        })
                        .to_string(),
                    )
                    .unwrap(),
                );
            }
        }
        bot.mark_request_as_sent(&request.txn_id, &send_event_to_device::v3::Response::new())
            .await
            .unwrap();
    }
    assert!(
        !events.is_empty(),
        "unverified reader must receive an encrypted key"
    );
    let decryption = DecryptionSettings {
        sender_device_trust_requirement: TrustRequirement::Untrusted,
    };
    let (_, keys) = reader
        .receive_sync_changes(
            EncryptionSyncChanges {
                to_device_events: events,
                changed_devices: &Default::default(),
                one_time_keys_counts: &Default::default(),
                unused_fallback_keys: None,
                next_batch_token: None,
            },
            &decryption,
        )
        .await
        .unwrap();
    assert_eq!(keys.len(), 1);
    let content = ruma::events::room::message::RoomMessageEventContent::text_plain(
        "Readable encrypted reply",
    );
    let encrypted = bot
        .encrypt_room_event(room, AnyMessageLikeEventContent::RoomMessage(content))
        .await
        .unwrap();
    let event = Raw::from_json_string(
        json!({
            "event_id":"$reply", "origin_server_ts":1, "sender":bot.user_id(),
            "type":"m.room.encrypted", "content":encrypted.content
        })
        .to_string(),
    )
    .unwrap();
    assert!(!event.json().get().contains("Readable encrypted reply"));
    let decrypted = reader
        .decrypt_room_event(&event, room, &decryption)
        .await
        .unwrap();
    let decoded = decode(
        room.as_str(),
        &TimelineEvent::from_decrypted(decrypted, None),
    )
    .unwrap()
    .unwrap();
    assert_eq!(decoded.body, "Readable encrypted reply");
    assert!(
        !device.is_verified(),
        "reading must not silently trust the reader for commands"
    );

    device
        .set_local_trust(LocalTrust::BlackListed)
        .await
        .unwrap();
    let blocked = bot
        .share_room_key(
            room,
            std::iter::once(reader.user_id()),
            EncryptionSettings {
                sharing_strategy: REPLY_KEY_RECIPIENT_STRATEGY,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(
        blocked
            .iter()
            .all(|r| r.event_type.to_string() != "m.room.encrypted")
    );
    assert!(
        blocked
            .iter()
            .any(|r| r.event_type.to_string() == "m.room_key.withheld")
    );
}

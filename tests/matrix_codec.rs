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
